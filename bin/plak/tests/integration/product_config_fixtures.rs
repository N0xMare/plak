//! Load every testing scenario home under `configs/scenarios/`.
//!
//! Keeps the checked-in operator examples aligned with the current configuration
//! `ProductConfig` shape: one `config.toml` plus one template file per agent.

#![allow(
    missing_docs,
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "integration test crate may unwrap"
)]

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use plak::home::paths::PlakPaths;
use plak::product_config::{ProductCompositionSelection, ProductConfig};
use plakous_hosted::agent_tree_protocol::Tier;

/// Every scenario home, by directory name.
const SCENARIOS: &[&str] = &[
    "comp-direct",
    "comp-js-ptc",
    "comp-native-ptc",
    "comp-py-ptc",
    "fresh-home-bootstrap",
    "openrouter-free-only",
    "single-claude-opus-low",
    "single-claude-sonnet-low",
    "single-codex-luna-low",
    "single-codex-sol-low",
    "single-grok-46-low",
    "single-openrouter-free",
    "tree-homogeneous",
    "tree-mixed-oauth",
    "tree-openrouter-children",
    "tree-openrouter-root",
];

/// The one free `OpenRouter` model the scenario set pins.
const FREE_MODEL: &str = "nvidia/nemotron-3-ultra-550b-a55b:free";

fn scenarios_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../configs/scenarios")
}

fn scenario_dirs(dir: &Path) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap_or_else(|err| panic!("scenario dir {}: {err}", dir.display()))
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.join("config.toml").is_file())
        .collect();
    paths.sort();
    paths
}

fn load_scenario(dir: &Path) -> ProductConfig {
    let mut texts = vec![fs::read_to_string(dir.join("config.toml")).unwrap()];
    for entry in fs::read_dir(dir.join("agents"))
        .unwrap()
        .filter_map(Result::ok)
    {
        texts.push(fs::read_to_string(entry.path()).unwrap());
    }
    for text in &texts {
        assert!(
            !text.contains("gpt-5.3-codex-spark"),
            "{} must not pin Codex Spark",
            dir.display()
        );
        assert!(
            !text.contains("gpt-5.4-mini"),
            "{} must not pin gpt-5.4-mini",
            dir.display()
        );
    }
    let paths = PlakPaths::resolve(Some(OsStr::new(dir)), None).expect("scenario home");
    ProductConfig::load(&paths).unwrap_or_else(|err| panic!("load {}: {err}", dir.display()))
}

fn load(name: &str) -> ProductConfig {
    load_scenario(&scenarios_dir().join(name))
}

#[test]
fn every_named_scenario_exists_and_nothing_else_does() {
    let dir = scenarios_dir();
    let found: BTreeSet<String> = scenario_dirs(&dir)
        .iter()
        .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    let expected: BTreeSet<String> = SCENARIOS.iter().map(|name| (*name).to_owned()).collect();
    assert_eq!(
        found, expected,
        "configs/scenarios drifted from the list in this test"
    );
}

#[test]
fn every_scenario_loads_and_documents_itself() {
    let dir = scenarios_dir();
    for name in SCENARIOS {
        let scenario = dir.join(name);
        let cfg = load_scenario(&scenario);
        assert!(cfg.root_agent.is_some(), "{name} must set [root] agent");
        assert!(
            cfg.root_template().is_some(),
            "{name} [root] agent must resolve to a template file"
        );
        for template in cfg.working_set.names() {
            assert!(
                cfg.templates.contains_key(template.as_str()),
                "{name} working set names a missing template {template}"
            );
        }
        let config = fs::read_to_string(scenario.join("config.toml"))
            .unwrap_or_else(|err| panic!("{name}/config.toml: {err}"));
        assert!(
            config.starts_with(&format!("# Scenario: {name}.")),
            "{name}/config.toml must name the scenario in its opening comment"
        );
    }
}

#[test]
fn every_tree_scenario_has_a_spawning_root_and_a_reviewer_below_mutate() {
    for name in SCENARIOS
        .iter()
        .filter(|name| name.starts_with("tree-") || **name == "openrouter-free-only")
    {
        let cfg = load(name);
        let root = cfg.root_template().expect("root template");
        // The root spawns the working set; its own `spawns` would apply only
        // where it ran as a child, and the root's grant always adopts.
        assert!(
            !cfg.working_set.is_empty(),
            "{name} root must be able to spawn"
        );
        assert!(root.spawns.is_empty(), "{name} root is not a nested card");
        assert!(
            cfg.subagents,
            "{name} must leave the topology tools compiled"
        );
        let reviewer = cfg
            .working_set_template("reviewer")
            .unwrap_or_else(|| panic!("{name} names no reviewer"));
        assert_eq!(
            reviewer.max_tier,
            Tier::Execute,
            "{name} reviewer must be clipped below mutate"
        );
        let implementer = cfg
            .working_set_template("implementer")
            .unwrap_or_else(|| panic!("{name} names no implementer"));
        assert_eq!(implementer.max_tier, Tier::Mutate);
        assert!(
            implementer.spawns.is_empty(),
            "{name} implementer is a leaf"
        );
    }
}

#[test]
fn tree_homogeneous_puts_one_tuple_on_every_node() {
    let cfg = load("tree-homogeneous");
    let lead = cfg.root_template().expect("lead root");
    // the root's menu is the working set, so the workers are separate
    // templates on the same tuple.
    assert!(!cfg.working_set.contains("lead"));
    for template in [
        lead,
        cfg.working_set_template("implementer")
            .expect("implementer"),
        cfg.working_set_template("reviewer").expect("reviewer"),
    ] {
        assert_eq!(template.account, "codex");
        assert_eq!(template.model, "gpt-5.6-luna");
        assert_eq!(template.effort.as_deref(), Some("low"));
        assert_eq!(
            template.composition_or_default(),
            ProductCompositionSelection::FollowCatalog
        );
    }
}

#[test]
fn tree_mixed_oauth_spans_three_subscriptions_with_tier_ceilings() {
    let cfg = load("tree-mixed-oauth");
    let lead = cfg.root_template().expect("lead root");
    assert_eq!(lead.account, "codex");
    assert_eq!(lead.model, "gpt-5.6-luna");

    let implement = cfg
        .working_set_template("implementer")
        .expect("implementer template");
    assert_eq!(implement.account, "codex");
    assert_eq!(implement.max_tier, Tier::Mutate);

    let review = cfg
        .working_set_template("reviewer")
        .expect("reviewer template");
    assert_eq!(review.account, "claude");
    assert_eq!(review.model, "claude-sonnet-5");
    assert_eq!(review.max_tier, Tier::Execute);
    assert_eq!(
        review.composition_or_default(),
        ProductCompositionSelection::Direct
    );

    let scout = cfg.working_set_template("scout").expect("scout template");
    assert_eq!(scout.account, "grok");
    assert_eq!(scout.model, "grok-4.6");
    assert_eq!(scout.max_tier, Tier::Execute);
    assert_eq!(
        scout.composition_or_default(),
        ProductCompositionSelection::Direct
    );
}

#[test]
fn the_openrouter_scenarios_bind_one_free_model_on_direct() {
    // native-ptc is refused for openrouter, so every openrouter template here
    // must say direct explicitly.
    let root = load("tree-openrouter-root");
    let lead = root.root_template().expect("openrouter lead");
    assert_eq!(lead.account, "openrouter");
    assert_eq!(lead.model, FREE_MODEL);
    assert_eq!(
        lead.composition_or_default(),
        ProductCompositionSelection::Direct
    );
    assert!(
        !root.working_set.is_empty(),
        "an openrouter root can bind children"
    );
    assert_eq!(
        root.working_set_template("implementer")
            .expect("implementer")
            .account,
        "codex"
    );
    assert_eq!(
        root.working_set_template("reviewer")
            .expect("reviewer")
            .account,
        "claude"
    );

    let children = load("tree-openrouter-children");
    assert_eq!(
        children.root_template().expect("codex lead").account,
        "codex"
    );
    for name in ["implementer", "reviewer"] {
        let template = children.working_set_template(name).expect("free child");
        assert_eq!(template.account, "openrouter");
        assert_eq!(template.model, FREE_MODEL);
        assert_eq!(
            template.composition_or_default(),
            ProductCompositionSelection::Direct
        );
    }

    // The only tree that spends no subscription quota anywhere.
    let free = load("openrouter-free-only");
    let mut templates = vec![free.root_template().expect("free lead")];
    for name in ["implementer", "reviewer"] {
        templates.push(free.working_set_template(name).expect("free child"));
    }
    for template in templates {
        assert_eq!(template.account, "openrouter");
        assert_eq!(template.model, FREE_MODEL);
        assert_eq!(template.effort, None, "OpenRouter Chat attests no effort");
        assert_eq!(
            template.composition_or_default(),
            ProductCompositionSelection::Direct
        );
    }

    let single = load("single-openrouter-free");
    let solo = single.root_template().expect("free root");
    assert_eq!(solo.model, FREE_MODEL);
    assert!(single.working_set.is_empty());
}

#[test]
fn the_single_scenarios_are_one_tuple_each_with_no_roster() {
    for (name, account, model) in [
        ("single-codex-luna-low", "codex", "gpt-5.6-luna"),
        ("single-codex-sol-low", "codex", "gpt-5.6-sol"),
        ("single-grok-46-low", "supergrok", "grok-4.6"),
        ("single-claude-sonnet-low", "claude", "claude-sonnet-5"),
        ("single-claude-opus-low", "claude", "claude-opus-5"),
    ] {
        let cfg = load(name);
        let template = cfg.root_template().expect("root template");
        assert_eq!(template.account, account, "{name} account");
        assert_eq!(template.model, model, "{name} model");
        assert_eq!(template.effort.as_deref(), Some("low"), "{name} effort");
        assert!(cfg.working_set.is_empty(), "{name} must name no roster");
    }

    // Grok is pinned to direct; the two plakous-default lanes leave it unset.
    assert_eq!(
        load("single-grok-46-low")
            .root_template()
            .expect("grok")
            .composition_or_default(),
        ProductCompositionSelection::Direct
    );
    for name in ["single-claude-sonnet-low", "single-claude-opus-low"] {
        assert_eq!(
            load(name).root_template().expect("claude").composition,
            None,
            "{name} leaves composition at the catalog default"
        );
    }
}

#[test]
fn the_comp_scenarios_move_only_the_composition() {
    for (name, composition) in [
        ("comp-direct", ProductCompositionSelection::Direct),
        ("comp-native-ptc", ProductCompositionSelection::NativePtc),
        ("comp-js-ptc", ProductCompositionSelection::JsPtc),
        ("comp-py-ptc", ProductCompositionSelection::PyPtc),
    ] {
        let cfg = load(name);
        let template = cfg.root_template().expect("root template");
        assert_eq!(template.account, "codex", "{name} holds the control tuple");
        assert_eq!(
            template.model, "gpt-5.6-luna",
            "{name} holds the control tuple"
        );
        assert_eq!(template.effort.as_deref(), Some("low"));
        assert_eq!(template.composition_or_default(), composition, "{name}");
        assert!(cfg.working_set.is_empty());
    }
}

#[test]
fn fresh_home_bootstrap_is_the_minimum_a_newcomer_copies() {
    let cfg = load("fresh-home-bootstrap");
    let lead = cfg.root_template().expect("lead");
    assert_eq!(lead.account, "codex");
    assert_eq!(lead.model, "gpt-5.6-luna");
    assert!(cfg.working_set.is_empty(), "no roster to maintain");
    assert!(lead.spawns.is_empty(), "delegation is opt-in");
    assert_eq!(cfg.templates.len(), 1, "one template, one file");
    // Every other key stays at its schema default.
    let defaults = ProductConfig::default_product();
    assert_eq!(cfg.subagents, defaults.subagents);
    assert_eq!(cfg.max_depth, defaults.max_depth);
    assert_eq!(cfg.max_children, defaults.max_children);
    assert_eq!(cfg.max_nodes, defaults.max_nodes);
    assert_eq!(cfg.occupancy, defaults.occupancy);
    assert_eq!(cfg.compaction_enabled, defaults.compaction_enabled);
}
