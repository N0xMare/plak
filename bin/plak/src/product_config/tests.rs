#![allow(clippy::unwrap_used, reason = "unit tests")]

#[test]
fn transient_resumes_is_bounded_home_scoped_and_roundtrips() {
    assert_eq!(ProductConfig::default_product().transient_resumes, 1);
    for value in 0..=2 {
        let parsed =
            ProductConfig::parse(&format!("[orchestration]\ntransient_resumes = {value}\n"))
                .unwrap();
        assert_eq!(parsed.transient_resumes, value);
        let rendered = parsed.render();
        assert_eq!(
            ProductConfig::parse(&rendered).unwrap().transient_resumes,
            value
        );
    }
    assert!(ProductConfig::parse("[orchestration]\ntransient_resumes = 3\n").is_err());
    let mut assigned = ProductConfig::default_product();
    assigned
        .assign_key("orchestration.transient_resumes", "0")
        .unwrap();
    assert_eq!(assigned.transient_resumes, 0);
}
use super::*;
use proptest::prelude::*;

fn template(account: &str, model: &str) -> AgentTemplate {
    AgentTemplate {
        account: account.to_owned(),
        model: model.to_owned(),
        description: format!("{model} worker"),
        ..AgentTemplate::default()
    }
}

/// A global home plus a separate project root; trusting the project is
/// each test's explicit step so the untrusted path is the default.
fn project_fixture() -> (tempfile::TempDir, tempfile::TempDir, PlakPaths) {
    let temp = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let paths = PlakPaths::explicit(temp.path().join("home"))
        .with_project_root(project.path().to_path_buf());
    paths.ensure_layout().unwrap();
    (temp, project, paths)
}

fn write_template_file(dir: &Path, name: &str, model: &str) {
    fs::create_dir_all(dir).unwrap();
    fs::write(
        dir.join(format!("{name}.toml")),
        format!("account = \"codex\"\nmodel = \"{model}\"\ndescription = \"{name} worker\"\n"),
    )
    .unwrap();
}

fn project_home(project: &tempfile::TempDir) -> PathBuf {
    project.path().join(".plak")
}

/// The starter config tells the operator to uncomment a preference; doing
/// so must land the key in its section, not at top level.
#[test]
fn init_template_uncommenting_one_key_loads_it_in_its_section() {
    let temp = tempfile::tempdir().unwrap();
    let paths = PlakPaths::explicit(temp.path().join("home"));
    assert!(init_if_missing(&paths).unwrap());
    let template = fs::read_to_string(paths.config_file()).unwrap();
    assert!(ProductConfig::load(&paths).unwrap().tui_mouse);
    assert_eq!(template.matches("\n# mouse = true\n").count(), 1);
    fs::write(
        paths.config_file(),
        template.replace("\n# mouse = true\n", "\nmouse = false\n"),
    )
    .unwrap();
    assert!(!ProductConfig::load(&paths).unwrap().tui_mouse);
}

#[test]
fn untrusted_project_applies_nothing_and_names_the_blocked_source() {
    let (_temp, project, paths) = project_fixture();
    write_template_file(&paths.user_agents_dir(), "ga", "global-model");
    fs::write(paths.config_file(), "[root]\nagent = \"ga\"\n").unwrap();
    let home = project_home(&project);
    write_template_file(&home.join("agents"), "pt", "project-model");
    fs::write(home.join("config.toml"), "[tui]\nmouse = false\n").unwrap();

    let config = ProductConfig::load(&paths).unwrap();
    assert!(config.tui_mouse, "untrusted keys must not apply");
    assert!(!config.templates.contains_key("pt"));
    let source = config.project.as_ref().unwrap();
    assert!(!source.trusted);
    assert_eq!(
        source.root,
        project.path().canonicalize().unwrap(),
        "the recorded root is the canonical project root"
    );
    let warning = config.warnings.join("\n");
    assert!(warning.contains("not trusted"), "{warning}");
    assert!(
        warning.contains("config.toml") && warning.contains("agents/"),
        "{warning}"
    );
}

#[test]
fn trusted_project_overlays_keys_and_replaces_whole_templates() {
    let (_temp, project, paths) = project_fixture();
    write_template_file(&paths.user_agents_dir(), "ga", "global-model");
    fs::write(
        paths.config_file(),
        "[root]\nagent = \"ga\"\n[tui]\nmouse = true\n",
    )
    .unwrap();
    let home = project_home(&project);
    write_template_file(&home.join("agents"), "ga", "project-model");
    write_template_file(&home.join("agents"), "pt", "project-only");
    fs::write(
        home.join("config.toml"),
        "[root]\nagent = \"pt\"\n[tui]\nmouse = false\n",
    )
    .unwrap();
    project_trust::trust(&paths, project.path()).unwrap();

    let config = ProductConfig::load(&paths).unwrap();
    assert!(!config.tui_mouse);
    assert_eq!(config.root_agent.as_deref(), Some("pt"));
    assert_eq!(config.templates["ga"].model, "project-model");
    assert_eq!(
        config.origin(ConfigField::TuiMouse),
        ConfigValueOrigin::Project
    );
    assert_eq!(
        config.origin(ConfigField::RootAgent),
        ConfigValueOrigin::Project
    );
    assert_eq!(config.template_origin("ga"), ConfigValueOrigin::Project);
    assert_eq!(config.template_origin("pt"), ConfigValueOrigin::Project);
    assert!(config.project.as_ref().unwrap().trusted);
}

#[test]
fn trusted_project_cannot_override_home_orchestration() {
    let (_temp, project, paths) = project_fixture();
    fs::write(
        paths.config_file(),
        "[orchestration]\nsubagents = false\nmax_depth = 2\nmax_children = 3\nmax_nodes = 8\nmax_attempts = 12\ntransient_resumes = 0\nmax_concurrent_requests_per_account = 3\nrequire_review_evidence = true\n[turn]\nauto_continue = false\n[mcp]\nclient_id_metadata_url = \"https://home.example/plak/client.json\"\n",
    )
    .unwrap();
    let home = project_home(&project);
    fs::create_dir_all(&home).unwrap();
    fs::write(
        home.join("config.toml"),
        "[orchestration]\nsubagents = true\nmax_depth = 4\nmax_children = 8\nmax_nodes = 32\nmax_attempts = 64\ntransient_resumes = 2\nmax_concurrent_requests_per_account = 9\nrequire_review_evidence = false\n[turn]\nauto_continue = true\n[tui]\nmouse = false\n[mcp]\nclient_id_metadata_url = \"https://project.example/client.json\"\n",
    )
    .unwrap();
    project_trust::trust(&paths, project.path()).unwrap();

    let global = ProductConfig::load_user(&paths).unwrap();
    let config = ProductConfig::load(&paths).unwrap();
    for row in CONFIG_KEYS.iter().filter(|row| row.field.is_home_only()) {
        assert_eq!(
            config.field_value(row.field).map(|value| value.to_string()),
            global.field_value(row.field).map(|value| value.to_string())
        );
        assert_eq!(config.origin(row.field), ConfigValueOrigin::User);
    }
    assert!(
        !config.tui_mouse,
        "ordinary project preferences still apply"
    );
    assert!(!SessionPolicy::load(&paths).unwrap().topology_enabled);
    let displayed = config.display_redacted(&paths);
    assert!(displayed.contains("[orchestration] is home-scoped and ignored here"));
    assert!(displayed.contains("[mcp] client_id_metadata_url is home-scoped and ignored here"));
    assert!(displayed.contains("origin.orchestration.subagents = user"));
}

#[test]
fn project_save_refuses_orchestration_changes_before_publication() {
    let (_temp, project, paths) = project_fixture();
    fs::write(paths.config_file(), "[orchestration]\nsubagents = false\n").unwrap();
    let home = project_home(&project);
    fs::create_dir_all(&home).unwrap();
    let project_file = home.join("config.toml");
    fs::write(&project_file, "[tui]\nmouse = false\n").unwrap();
    project_trust::trust(&paths, project.path()).unwrap();
    let before = fs::read(&project_file).unwrap();
    let mut config = ProductConfig::load(&paths).unwrap();
    config.subagents = true;
    let error = config
        .save_scoped(&paths, ConfigWriteScope::Project)
        .unwrap_err();
    assert!(
        error
            .into_message()
            .contains("orchestration.subagents is home-scoped")
    );
    assert_eq!(fs::read(project_file).unwrap(), before);
    assert!(!ProductConfig::load_user(&paths).unwrap().subagents);
}

#[test]
fn project_config_must_supply_the_templates_it_names() {
    let (_temp, project, paths) = project_fixture();
    fs::write(paths.config_file(), "").unwrap();
    let home = project_home(&project);
    fs::create_dir_all(&home).unwrap();
    fs::write(home.join("config.toml"), "[root]\nagent = \"missing\"\n").unwrap();
    project_trust::trust(&paths, project.path()).unwrap();
    let error = ProductConfig::load(&paths).unwrap_err();
    assert!(error.contains("missing.toml"), "{error}");
    assert!(error.contains(".plak"), "{error}");
}

#[test]
fn global_config_must_resolve_globally_even_when_a_project_could() {
    let (_temp, project, paths) = project_fixture();
    fs::write(paths.config_file(), "[root]\nagent = \"only-project\"\n").unwrap();
    let home = project_home(&project);
    write_template_file(&home.join("agents"), "only-project", "project-model");
    project_trust::trust(&paths, project.path()).unwrap();
    // A project template cannot satisfy a global reference: the error is
    // the global file's own unresolved name.
    let error = ProductConfig::load(&paths).unwrap_err();
    assert!(error.contains("only-project.toml"), "{error}");
}

#[test]
fn user_scope_save_never_flattens_project_values() {
    let (_temp, project, paths) = project_fixture();
    write_template_file(&paths.user_agents_dir(), "ga", "global-model");
    fs::write(
        paths.config_file(),
        "[root]\nagent = \"ga\"\n[tui]\ntheme = \"default-dark\"\n",
    )
    .unwrap();
    let home = project_home(&project);
    write_template_file(&home.join("agents"), "pt", "project-model");
    fs::write(
        home.join("config.toml"),
        "[tui]\ntheme = \"catppuccin-mocha\"\n",
    )
    .unwrap();
    project_trust::trust(&paths, project.path()).unwrap();

    let mut config = ProductConfig::load(&paths).unwrap();
    config.set_key("tui.scroll_lines", "7").unwrap();
    config.save_scoped(&paths, ConfigWriteScope::User).unwrap();
    let global_text = fs::read_to_string(paths.config_file()).unwrap();
    assert!(global_text.contains("scroll_lines = 7"), "{global_text}");
    assert!(
        global_text.contains("theme = \"default-dark\""),
        "the global file keeps its own theme: {global_text}"
    );
    assert!(
        !paths.user_agents_dir().join("pt.toml").exists(),
        "a project template never lands in the user directory"
    );
    // The merged view is untouched: reload still shows the project theme.
    let reloaded = ProductConfig::load(&paths).unwrap();
    assert_eq!(reloaded.theme, ProductTheme::CatppuccinMocha);
}

#[test]
fn project_scope_save_writes_only_the_delta() {
    let (_temp, project, paths) = project_fixture();
    write_template_file(&paths.user_agents_dir(), "ga", "global-model");
    fs::write(
        paths.config_file(),
        "[root]\nagent = \"ga\"\n[tui]\ntheme = \"default-dark\"\n",
    )
    .unwrap();
    let home = project_home(&project);
    fs::create_dir_all(&home).unwrap();
    project_trust::trust(&paths, project.path()).unwrap();
    let global_before = fs::read(paths.config_file()).unwrap();

    let mut config = ProductConfig::load(&paths).unwrap();
    config.set_key("tui.theme", "light").unwrap();
    config
        .save_scoped(&paths, ConfigWriteScope::Project)
        .unwrap();
    let project_text = fs::read_to_string(home.join("config.toml")).unwrap();
    assert!(
        project_text.contains("theme = \"default-light\""),
        "{project_text}"
    );
    assert!(
        !project_text.contains("scroll_lines") && !project_text.contains("[root]"),
        "the override file carries only the delta: {project_text}"
    );
    assert_eq!(fs::read(paths.config_file()).unwrap(), global_before);
    // And the merged load now shows the override.
    let reloaded = ProductConfig::load(&paths).unwrap();
    assert_eq!(reloaded.theme, ProductTheme::DefaultLight);
    assert_eq!(
        reloaded.origin(ConfigField::Theme),
        ConfigValueOrigin::Project
    );
}

#[test]
fn project_scope_save_requires_trust() {
    let (_temp, project, paths) = project_fixture();
    fs::write(paths.config_file(), "").unwrap();
    fs::create_dir_all(project_home(&project)).unwrap();
    let mut config = ProductConfig::load(&paths).unwrap();
    config.set_key("tui.scroll_lines", "5").unwrap();
    let error = config
        .save_scoped(&paths, ConfigWriteScope::Project)
        .unwrap_err();
    let message = error.into_message();
    assert!(message.contains("not trusted"), "{message}");
    assert!(message.contains("plak config trust"), "{message}");
}

#[test]
fn launching_in_the_home_parent_does_not_read_home_as_a_project() {
    let temp = tempfile::tempdir().unwrap();
    let paths =
        PlakPaths::explicit(temp.path().join(".plak")).with_project_root(temp.path().to_path_buf());
    paths.ensure_layout().unwrap();
    write_template_file(&paths.user_agents_dir(), "ga", "global-model");
    fs::write(paths.config_file(), "[root]\nagent = \"ga\"\n").unwrap();
    let config = ProductConfig::load(&paths).unwrap();
    assert!(config.project.is_none());
    assert_eq!(config.root_agent.as_deref(), Some("ga"));
}

#[test]
fn the_theme_set_is_the_six_the_tui_ships() {
    let shipped = [
        ProductTheme::DefaultDark,
        ProductTheme::DefaultLight,
        ProductTheme::CatppuccinLatte,
        ProductTheme::CatppuccinFrappe,
        ProductTheme::CatppuccinMacchiato,
        ProductTheme::CatppuccinMocha,
    ];
    for (host, tui) in shipped
        .into_iter()
        .zip(crate::surface_protocol::SettingsTheme::ALL)
    {
        assert_eq!(host.as_str(), tui.as_str());
        assert_eq!(ProductTheme::parse(host.as_str()), Ok(host));
    }
    // `dark` and `light` remain accepted aliases of the two defaults.
    assert_eq!(ProductTheme::parse("dark"), Ok(ProductTheme::DefaultDark));
    assert_eq!(ProductTheme::parse("light"), Ok(ProductTheme::DefaultLight));
}

#[test]
fn the_new_tui_rows_round_trip_through_the_file() {
    let mut config = ProductConfig::default_product();
    config.set_key("tui.scroll_lines", "7").unwrap();
    config.set_key("tui.show_thinking", "false").unwrap();
    config.set_key("tui.group_tool_calls", "false").unwrap();
    config.set_key("tui.reduced_motion", "true").unwrap();
    config.set_key("tui.theme", "default-light").unwrap();
    let round_tripped = ProductConfig::parse(&config.render()).unwrap();
    assert_eq!(round_tripped, config);
    assert_eq!(round_tripped.scroll_lines, 7);
    assert!(!round_tripped.show_thinking);
    assert!(!round_tripped.group_tool_calls);
    assert!(round_tripped.reduced_motion);
    assert_eq!(round_tripped.theme, ProductTheme::DefaultLight);
}

fn catalog_entry(
    id: &str,
    efforts: &[&str],
    extras: &[&str],
) -> plakous_host_protocol::views::ModelEntry {
    plakous_host_protocol::views::ModelEntry {
        id: id.to_owned(),
        tag: String::new(),
        reasoning: !efforts.is_empty(),
        effort_options: efforts.iter().map(|value| (*value).to_owned()).collect(),
        extras: extras
            .iter()
            .map(|value| plakous_host_protocol::views::ExtraState {
                id: (*value).to_owned(),
                label: (*value).to_owned(),
                enabled: false,
            })
            .collect(),
        unavailable_reason: None,
    }
}

fn catalog(
    account: &str,
    ready: bool,
    entries: Vec<plakous_host_protocol::views::ModelEntry>,
) -> Vec<plakous_host_protocol::views::ProviderSection> {
    vec![plakous_host_protocol::views::ProviderSection {
        account: account.to_owned(),
        label: account.to_owned(),
        ready,
        entries,
    }]
}

#[test]
fn protocol_selection_is_closed_and_auto_resolves_once_to_chat() {
    for (text, selection) in [
        ("auto", ProductProtocolSelection::Auto),
        ("chat", ProductProtocolSelection::Chat),
    ] {
        assert_eq!(ProductProtocolSelection::parse(text), Ok(selection));
        assert_eq!(selection.as_str(), text);
        let binding = ProductWireBinding::resolve(selection);
        assert_eq!(binding.resolved, ProductResolvedProtocol::Chat);
        assert_eq!(
            binding.resolution_revision,
            PRODUCT_WIRE_RESOLUTION_REVISION
        );
        binding.validate().unwrap();
    }
    for (text, selection, resolved) in [
        (
            "responses",
            ProductProtocolSelection::Responses,
            ProductResolvedProtocol::Responses,
        ),
        (
            "messages",
            ProductProtocolSelection::Messages,
            ProductResolvedProtocol::Messages,
        ),
    ] {
        assert_eq!(ProductProtocolSelection::parse(text), Ok(selection));
        assert_eq!(selection.as_str(), text);
        let binding = ProductWireBinding::resolve(selection);
        assert_eq!(binding.resolved, resolved);
        binding.validate().unwrap();
    }
    for rejected in ["", "Auto", "completion", "chat|responses"] {
        assert!(ProductProtocolSelection::parse(rejected).is_err());
    }
}

#[test]
fn wire_binding_rejects_reresolution_and_revision_drift() {
    let mut binding = ProductWireBinding::resolve(ProductProtocolSelection::Auto);
    binding.resolved = ProductResolvedProtocol::Responses;
    assert!(binding.validate().is_err());
    binding.resolved = ProductResolvedProtocol::Chat;
    binding.resolution_revision = "future-default".into();
    assert!(binding.validate().is_err());

    let contradictory = ProductWireBinding {
        selection: ProductProtocolSelection::Responses,
        resolved: ProductResolvedProtocol::Messages,
        resolution_revision: PRODUCT_WIRE_RESOLUTION_REVISION.into(),
    };
    assert_eq!(
        contradictory.validate().unwrap_err(),
        "wire binding contradicts its selected protocol"
    );
}

#[test]
fn openrouter_routing_and_target_shape_validate_exactly() {
    let target = ProductTargetSelection::OpenRouter {
        wire: ProductWireBinding::resolve(ProductProtocolSelection::Auto),
        routing: ProductOpenRouterRoutingSelection::GatewayManaged,
    };
    target.validate().unwrap();
    let encoded = serde_json::to_string(&target).unwrap();
    assert_eq!(
        serde_json::from_str::<ProductTargetSelection>(&encoded).unwrap(),
        target
    );
}

#[test]
fn telemetry_section_round_trips_through_render_and_parse() {
    let mut cfg = ProductConfig::default_product();
    cfg.telemetry_max_bytes = 42_000_000;
    cfg.telemetry_max_age_days = 14;
    let text = cfg.render();
    let back = ProductConfig::parse(&text).unwrap();
    assert_eq!(back.telemetry_max_bytes, 42_000_000);
    assert_eq!(back.telemetry_max_age_days, 14);
}

#[test]
fn telemetry_bounds_are_settable_and_refuse_zero() {
    let mut cfg = ProductConfig::default_product();
    cfg.assign_key("telemetry.max_bytes", "1000").unwrap();
    cfg.assign_key("telemetry.max_age_days", "7").unwrap();
    assert_eq!(cfg.telemetry_max_bytes, 1000);
    assert_eq!(cfg.telemetry_max_age_days, 7);
    assert!(cfg.assign_key("telemetry.max_bytes", "0").is_err());
    assert!(cfg.assign_key("telemetry.max_age_days", "-1").is_err());
    let zero = cfg.render().replace("max_age_days = 7", "max_age_days = 0");
    assert!(ProductConfig::parse(&zero).is_err());
}

#[test]
fn composition_selector_is_closed_and_canonical() {
    for (value, expected) in [
        ("direct", ProductCompositionSelection::Direct),
        (
            "plakous-default",
            ProductCompositionSelection::FollowCatalog,
        ),
        ("native-ptc", ProductCompositionSelection::NativePtc),
        ("js-ptc", ProductCompositionSelection::JsPtc),
        ("py-ptc", ProductCompositionSelection::PyPtc),
    ] {
        assert_eq!(ProductCompositionSelection::parse(value), Ok(expected));
        assert_eq!(
            ProductCompositionSelection::parse_config(value),
            Ok(expected)
        );
        assert_eq!(expected.as_str(), value);
    }
    for rejected in [
        "native",
        "javascript",
        "python",
        "follow-catalog",
        "follow_catalog",
        "js",
        "py",
    ] {
        assert!(ProductCompositionSelection::parse(rejected).is_err());
        assert!(ProductCompositionSelection::parse_config(rejected).is_err());
    }
}

// Template files, the working set, and the name grammar.

/// An isolated home whose `agents/` holds the given template files.
fn home_with_templates(files: &[(&str, &str)]) -> (tempfile::TempDir, PlakPaths) {
    let (tmp, paths) = PlakPaths::isolated_test_home();
    paths.ensure_layout().unwrap();
    for (name, body) in files {
        fs::write(paths.user_agents_dir().join(format!("{name}.toml")), body).unwrap();
    }
    (tmp, paths)
}

fn template_body(account: &str, model: &str, extra_lines: &str) -> String {
    format!(
        "account = \"{account}\"\nmodel = \"{model}\"\ndescription = \"{model} worker\"\n{extra_lines}"
    )
}

fn save_and_reload(cfg: &ProductConfig) -> ProductConfig {
    let (_tmp, paths) = PlakPaths::isolated_test_home();
    paths.ensure_layout().unwrap();
    cfg.save(&paths).unwrap();
    ProductConfig::load(&paths).unwrap()
}

#[test]
fn full_schema_round_trips_through_files() {
    let text = concat!(
        "[root]\nagent = \"router\"\n",
        "\n[roster]\n",
        "working_set = [\"fast-impl\", \"router\"]\n",
        "\n[orchestration]\n",
        "subagents = true\n",
        "max_depth = 2\n",
        "max_children = 3\n",
        "max_nodes = 9\n",
        "\n[tui]\nmouse = false\ntheme = \"catppuccin-mocha\"\nscroll_lines = 6\n",
        "\n[compaction]\nenabled = false\n",
    );
    let (_tmp, paths) = home_with_templates(&[
        (
            "fast-impl",
            &template_body(
                "codex",
                "gpt-5.6-luna",
                "effort = \"low\"\nfast_mode = true\ncomposition = \"native-ptc\"\naccess = \"full\"\ncost_tier = 3\n[env]\nCARGO_TARGET_DIR = \"/tmp/t\"\n",
            ),
        ),
        (
            "router",
            &template_body(
                "openrouter",
                "vendor/model",
                "composition = \"direct\"\nprotocol = \"chat\"\nspawns = [\"fast-impl\"]\nmay_adopt = true\ndelegation = \"proactive\"\nshow_model = true\nmax_turns = 12\non_mint = \"cargo fetch\"\n",
            ),
        ),
    ]);
    fs::write(paths.config_file(), text).unwrap();
    let parsed = ProductConfig::load(&paths).unwrap();
    assert_eq!(parsed.root_agent.as_deref(), Some("router"));
    assert_eq!(parsed.templates.len(), 2);
    assert_eq!(
        parsed
            .working_set
            .names()
            .iter()
            .map(TemplateName::as_str)
            .collect::<Vec<_>>(),
        ["fast-impl", "router"]
    );
    let fast = &parsed.templates["fast-impl"];
    assert_eq!(fast.cost_tier, Some(3));
    assert_eq!(
        fast.env.get("CARGO_TARGET_DIR").map(String::as_str),
        Some("/tmp/t")
    );
    let router = &parsed.templates["router"];
    assert_eq!(router.spawns, [TemplateName::parse("fast-impl").unwrap()]);
    assert!(router.may_adopt && router.show_model);
    assert_eq!(router.delegation, Delegation::Proactive);
    assert_eq!(router.max_turns, Some(12));
    assert_eq!(router.on_mint.as_deref(), Some("cargo fetch"));
    assert_eq!(parsed.max_children, 3);
    assert_eq!(parsed.scroll_lines, 6);
    let again = save_and_reload(&parsed);
    assert_eq!(again, parsed);
    assert_eq!(again.render(), parsed.render());
    for template in parsed.templates.values() {
        assert_eq!(
            parse_template(&render_template(template), "t").unwrap(),
            *template
        );
    }
}

#[test]
fn project_template_wins_over_the_user_file_on_collision() {
    let (tmp, paths) = home_with_templates(&[
        ("critic", &template_body("claude", "claude-opus-5", "")),
        ("scout", &template_body("codex", "gpt-5.6-luna", "")),
    ]);
    let project = tmp.path().join("project");
    fs::create_dir_all(project.join(".plak/agents")).unwrap();
    fs::write(
        project.join(".plak/agents/critic.toml"),
        template_body("grok", "grok-4.6", "access = \"read-execute\"\n"),
    )
    .unwrap();
    let paths = paths.with_project_root(project.clone());
    // Untrusted: the project file is discovered but not applied.
    let untrusted = ProductConfig::load(&paths).unwrap();
    assert_eq!(untrusted.templates["critic"].account, "claude");
    // Trusted: the same-name project file replaces the whole template.
    project_trust::trust(&paths, &project).unwrap();
    let loaded = ProductConfig::load(&paths).unwrap();
    assert_eq!(loaded.templates["critic"].account, "grok");
    assert_eq!(loaded.templates["critic"].max_tier, Tier::Execute);
    assert_eq!(loaded.template_origin("critic"), ConfigValueOrigin::Project);
    assert_eq!(loaded.templates["scout"].account, "codex");
    // A user-scope save cannot rewrite the project's file — the edited
    // project body simply is not user content.
    let mut edited = loaded.clone();
    edited.templates.get_mut("critic").unwrap().effort = Some("high".into());
    edited.save(&paths).unwrap();
    assert_eq!(
        parse_template(
            &fs::read_to_string(project.join(".plak/agents/critic.toml")).unwrap(),
            "critic"
        )
        .unwrap()
        .effort,
        None
    );
    // The ownership refusal lives in the project scope: a body that
    // disagrees with the file on disk is refused, not overwritten.
    let message = edited
        .save_scoped(&paths, ConfigWriteScope::Project)
        .unwrap_err()
        .into_message();
    assert!(message.contains("defined by the project"), "{message}");
    // An unchanged project template is not an error in either scope.
    loaded.save(&paths).unwrap();
    loaded
        .save_scoped(&paths, ConfigWriteScope::Project)
        .unwrap();
}

#[test]
fn template_names_accept_and_reject_at_their_boundaries() {
    for accepted in ["a", "a1", "fast-impl", "z-9", &"a".repeat(32)] {
        validate_template_name(accepted).unwrap();
    }
    for rejected in [
        "",
        "1a",
        "-a",
        "A",
        "a_b",
        "a.b",
        "a b",
        &"a".repeat(33),
        "root",
        "roster",
        "orchestration",
        "tui",
        "compaction",
        "telemetry",
        "agent",
    ] {
        assert!(
            validate_template_name(rejected).is_err(),
            "expected rejection: {rejected}"
        );
    }
    let (_tmp, paths) = home_with_templates(&[("Bad", &template_body("codex", "m", ""))]);
    assert!(
        ProductConfig::load(&paths)
            .unwrap_err()
            .contains("invalid template name")
    );
    let (_tmp, paths) = home_with_templates(&[("root", &template_body("codex", "m", ""))]);
    assert!(
        ProductConfig::load(&paths)
            .unwrap_err()
            .contains("reserved")
    );
    assert!(
        parse_template(
            "name = \"one\"\naccount = \"codex\"\nmodel = \"m\"\n",
            "one"
        )
        .unwrap_err()
        .contains("unknown key name")
    );
}

/// `[agent.*]` left in `config.toml` is a named error, not a
/// silent ignore.
#[test]
fn config_toml_agent_section_is_a_named_error() {
    let error = ProductConfig::parse("[agent.scout]\naccount = \"codex\"\n").unwrap_err();
    assert_eq!(
        error,
        "[agent.scout] in config.toml is no longer a template; move it to ~/.plak/agents/scout.toml (line 1, column 2)"
    );
}

#[test]
fn roster_key_other_than_working_set_is_a_named_error() {
    assert_eq!(
        ProductConfig::parse("[roster]\nreview = \"critic\"\n").unwrap_err(),
        "[roster] only accepts working_set; move review into agents/<name>.toml (line 2, column 1)"
    );
    assert!(
        ProductConfig::parse("[roster]\nworking_set = \"critic\"\n")
            .unwrap_err()
            .contains("must be an array")
    );
    let set = ProductConfig::parse("[roster]\nworking_set = [\"a\", \"b\"]\n").unwrap();
    assert_eq!(set.working_set.len(), 2);
    assert!(ProductConfig::parse("[roster]\nworking_set = [\"a\", \"a\"]\n").is_err());
    assert!(ProductConfig::parse("[roster]\nworking_set = []\n").is_ok());
}

/// `fast_mode` is the only spelling of the Codex fast lane: a bool that
/// maps to the provider-native `priority` option internally.
#[test]
fn fast_mode_is_a_codex_only_bool() {
    let fast = parse_template(
        "account = \"codex\"\nmodel = \"m\"\nfast_mode = true\n",
        "t",
    )
    .unwrap();
    assert_eq!(fast.extra.as_deref(), Some("priority"));
    assert!(fast.is_fast());
    let rendered = render_template(&fast);
    assert!(rendered.contains("fast_mode = true\n"), "{rendered}");
    assert_eq!(parse_template(&rendered, "t").unwrap(), fast);

    // false is explicit everywhere, including a non-Codex account.
    for account in ["codex", "grok"] {
        let standard = parse_template(
            &format!("account = \"{account}\"\nmodel = \"m\"\nfast_mode = false\n"),
            "t",
        )
        .unwrap();
        assert_eq!(standard.extra, None);
        assert!(!standard.is_fast());
        let rendered = render_template(&standard);
        assert!(rendered.contains("fast_mode = false\n"), "{rendered}");
        assert_eq!(parse_template(&rendered, "t").unwrap(), standard);
    }

    // true is refused on any other account.
    let error =
        parse_template("account = \"grok\"\nmodel = \"m\"\nfast_mode = true\n", "t").unwrap_err();
    assert!(
        error.contains("Fast mode requires the Codex subscription account"),
        "{error}"
    );
    let error = parse_template(
        "account = \"codex\"\nmodel = \"m\"\nfast_mode = \"true\"\n",
        "t",
    )
    .unwrap_err();
    assert!(error.contains("fast_mode must be a boolean"), "{error}");

    let error = parse_template(
        "account = \"codex\"\nmodel = \"m\"\ncomposition = \"follow-catalog\"\n",
        "t",
    )
    .unwrap_err();
    assert!(
        error.contains("direct|plakous-default|native-ptc|js-ptc|py-ptc"),
        "{error}"
    );
}

#[test]
fn every_template_validation_rule_has_a_positive_and_a_negative_case() {
    // Rule 1: account and model are required and non-empty.
    template("codex", "gpt").validate("t").unwrap();
    assert!(
        AgentTemplate {
            model: "gpt".into(),
            ..AgentTemplate::default()
        }
        .validate("t")
        .unwrap_err()
        .contains("requires a non-empty account")
    );
    assert!(
        template("codex", "")
            .validate("t")
            .unwrap_err()
            .contains("requires a non-empty model")
    );

    // Rule 2: the account set is closed.
    for account in [
        "codex",
        "chatgpt",
        "grok",
        "supergrok",
        "xai-oauth",
        "claude",
        "openrouter",
    ] {
        template(account, "m").validate("t").unwrap();
    }
    for account in ["openai", "anthropic", "gemini", ""] {
        assert!(template(account, "m").validate("t").is_err());
    }

    // Rule 3: protocol is OpenRouter-only.
    let mut openrouter = template("openrouter", "vendor/model");
    openrouter.protocol = Some(ProductProtocolSelection::Messages);
    openrouter.validate("t").unwrap();
    let mut native = template("codex", "gpt");
    native.protocol = Some(ProductProtocolSelection::Messages);
    assert!(
        native
            .validate("t")
            .unwrap_err()
            .contains("protocol is OpenRouter-only")
    );
    native.protocol = Some(ProductProtocolSelection::Auto);
    native.validate("t").unwrap();

    // Rule 4: OpenRouter enrolls no native PTC.
    let mut composed = template("openrouter", "vendor/model");
    composed.composition = Some(ProductCompositionSelection::JsPtc);
    composed.validate("t").unwrap();
    composed.composition = Some(ProductCompositionSelection::NativePtc);
    assert!(
        composed
            .validate("t")
            .unwrap_err()
            .contains("template t: OpenRouter has no native programmatic tool calling")
    );

    // Rule 5: fast_mode maps to `priority`, and only on codex.
    let mut fast = template("codex", "gpt");
    fast.extra = Some("priority".into());
    fast.validate("t").unwrap();
    fast.extra = Some("express".into());
    assert!(
        fast.validate("t")
            .unwrap_err()
            .contains("the provider option is unsupported")
    );
    let mut grok_fast = template("grok", "grok-4.6");
    grok_fast.extra = Some("priority".into());
    assert!(
        grok_fast
            .validate("t")
            .unwrap_err()
            .contains("Fast mode requires the Codex subscription account")
    );

    // Rule 6: any composition is admitted because the tuple is structural.
    for composition in [
        ProductCompositionSelection::Direct,
        ProductCompositionSelection::FollowCatalog,
        ProductCompositionSelection::NativePtc,
        ProductCompositionSelection::JsPtc,
        ProductCompositionSelection::PyPtc,
    ] {
        let mut record = template("codex", "gpt");
        record.composition = Some(composition);
        record.validate("t").unwrap();
    }

    // Rule 7: effort is opaque at parse.
    let mut effort = template("codex", "gpt");
    effort.effort = Some("bespoke".into());
    effort.validate("t").unwrap();

    // adoption authority presumes a subtree to adopt from.
    let mut adopter = template("codex", "gpt");
    adopter.may_adopt = true;
    assert_eq!(
        adopter.validate("t").unwrap_err(),
        "template t may_adopt requires spawns naming another template"
    );
    adopter.spawns = vec![TemplateName::parse("t").unwrap()];
    assert_eq!(
        adopter.validate("t").unwrap_err(),
        "template t may_adopt requires spawns naming another template"
    );
    adopter.spawns = vec![TemplateName::parse("w").unwrap()];
    adopter.validate("t").unwrap();
    // A spawns list is a set of at most five names.
    let mut nester = template("codex", "gpt");
    nester.spawns = ["a", "b", "a"]
        .map(|name| TemplateName::parse(name).unwrap())
        .to_vec();
    assert_eq!(
        nester.validate("t").unwrap_err(),
        "template t spawns names \"a\" more than once"
    );
    nester.spawns = ["a", "b", "c", "d", "e", "f"]
        .map(|name| TemplateName::parse(name).unwrap())
        .to_vec();
    assert_eq!(
        nester.validate("t").unwrap_err(),
        "template t spawns names 6 templates; the maximum is 5"
    );
    let mut turns = template("codex", "gpt");
    turns.max_turns = Some(0);
    assert!(turns.validate("t").is_err());
}

/// A direct API account's effort binds as written: it is required at load,
/// never defaulted at Fork now or Use now.
#[test]
fn a_direct_api_template_needs_an_effort_on_its_models_ladder_at_load() {
    let needs =
        "template t: openai model `gpt-5.6-sol` requires --effort none|low|medium|high|xhigh|max";
    for effort in ["", "effort = \"ultra\"\n"] {
        let text = format!("account = \"openai\"\nmodel = \"gpt-5.6-sol\"\n{effort}");
        assert_eq!(parse_template(&text, "t").unwrap_err(), needs, "{effort:?}");
    }
    for effort in ["none", "low", "medium", "high"] {
        let text =
            format!("account = \"openai\"\nmodel = \"gpt-5.6-sol\"\neffort = \"{effort}\"\n");
        let template = parse_template(&text, "t").unwrap();
        assert_eq!(template.effort.as_deref(), Some(effort));
    }
}

#[test]
fn template_file_parser_is_closed_and_typed() {
    assert!(
        parse_template("account = \"codex\"\nmodel = \"m\"\nsandbox = true\n", "t")
            .unwrap_err()
            .contains("unknown key sandbox")
    );
    assert!(
        parse_template(
            "account = \"codex\"\nmodel = \"m\"\naccess = \"root\"\n",
            "t"
        )
        .unwrap_err()
        .contains("access must be full|read-execute")
    );
    assert!(
        parse_template(
            "account = \"codex\"\nmodel = \"m\"\ndelegation = \"lazy\"\n",
            "t"
        )
        .unwrap_err()
        .contains("delegation must be")
    );
    assert!(
        parse_template("account = \"codex\"\nmodel = \"m\"\n[hooks]\nx = 1\n", "t")
            .unwrap_err()
            .contains("unknown section [hooks]")
    );
    // An unquoted value is not a TOML value; the document layer names the
    // line before the schema is consulted.
    let bare =
        parse_template("account = \"codex\"\nmodel = \"m\"\neffort = low\n", "t").unwrap_err();
    assert!(bare.starts_with("template t: "), "{bare}");
    assert!(bare.contains("line 3"), "{bare}");
    let quoted_boolean = parse_template(
        "account = \"codex\"\nmodel = \"m\"\nmay_adopt = \"true\"\n",
        "t",
    )
    .unwrap_err();
    assert!(
        quoted_boolean.contains("may_adopt must be a boolean"),
        "{quoted_boolean}"
    );
    assert!(quoted_boolean.contains("line 3"), "{quoted_boolean}");
    assert!(
        parse_template(
            "account = \"codex\"\nmodel = \"m\"\ncost_tier = \"3\"\n",
            "t"
        )
        .unwrap_err()
        .contains("cost_tier must be an integer")
    );
    assert!(
        parse_template("account = \"codex\"\nmodel = \"m\"\n[env]\nX = 1\n", "t")
            .unwrap_err()
            .contains("env X must be a string")
    );
    // Duplicate keys are the document layer's refusal, not a last-wins
    // silent overwrite.
    assert!(
        parse_template("account = \"codex\"\nmodel = \"m\"\nmodel = \"n\"\n", "t")
            .unwrap_err()
            .contains("duplicate key")
    );
    let defaults = parse_template("account = \"codex\"\nmodel = \"m\"\n", "t").unwrap();
    assert_eq!(defaults.max_tier, Tier::Mutate);
    assert!(defaults.spawns.is_empty() && !defaults.may_adopt && !defaults.show_model);
    assert_eq!(defaults.delegation, Delegation::Explicit);
    for bad in ["spawns = \"worker\"", "spawns = [1]"] {
        let error = parse_template(&format!("account = \"codex\"\nmodel = \"m\"\n{bad}\n"), "t")
            .unwrap_err();
        assert!(
            error.contains("spawns must be an array of template names"),
            "{error}"
        );
    }
    assert!(
        parse_template(
            "account = \"codex\"\nmodel = \"m\"\nspawns = [\"Root\"]\n",
            "t"
        )
        .unwrap_err()
        .contains("template t: spawns: ")
    );
    let nested = parse_template(
        "account = \"codex\"\nmodel = \"m\"\nspawns = [\"worker\", \"reviewer\"]\nmay_adopt = true\n",
        "t",
    )
    .unwrap();
    assert_eq!(
        nested
            .spawns
            .iter()
            .map(TemplateName::as_str)
            .collect::<Vec<_>>(),
        ["worker", "reviewer"]
    );
    let rendered = render_template(&nested);
    assert!(
        rendered.contains("spawns = [\"worker\", \"reviewer\"]\n"),
        "{rendered}"
    );
    assert_eq!(parse_template(&rendered, "t").unwrap(), nested);
}

#[test]
fn removed_sections_name_their_replacement() {
    let default_error = ProductConfig::parse("[default]\naccount = \"codex\"\n").unwrap_err();
    assert!(
        default_error.contains("template files under ~/.plak/agents"),
        "{default_error}"
    );
    let roster_error = ProductConfig::parse("[roster.review]\naccount = \"codex\"\n").unwrap_err();
    assert!(
        roster_error.contains("[roster] holds only working_set"),
        "{roster_error}"
    );
    assert_eq!(
        ProductConfig::parse("[sandbox]\non = true\n").unwrap_err(),
        "unknown config section [sandbox] (line 1, column 2)"
    );
}

#[test]
fn root_and_working_set_names_must_resolve_to_a_template_file() {
    let (_tmp, paths) = home_with_templates(&[("x", &template_body("codex", "m", ""))]);
    fs::write(paths.config_file(), "[root]\nagent = \"missing\"\n").unwrap();
    assert!(
        ProductConfig::load(&paths)
            .unwrap_err()
            .contains("[root] agent names no template file missing.toml")
    );
    fs::write(
        paths.config_file(),
        "[roster]\nworking_set = [\"missing\"]\n",
    )
    .unwrap();
    assert!(
        ProductConfig::load(&paths)
            .unwrap_err()
            .contains("[roster] working_set names no template file missing.toml")
    );
    fs::write(
        paths.config_file(),
        "[root]\nagent = \"x\"\n[roster]\nworking_set = [\"x\"]\n",
    )
    .unwrap();
    ProductConfig::load(&paths).unwrap();
}

/// A working-set member is rendered as a spawn card, so it needs the text
/// the model routes on. The root alone does not.
#[test]
fn working_set_members_need_a_description() {
    let (_tmp, paths) = home_with_templates(&[("quiet", "account = \"codex\"\nmodel = \"m\"\n")]);
    fs::write(paths.config_file(), "[root]\nagent = \"quiet\"\n").unwrap();
    ProductConfig::load(&paths).unwrap();
    fs::write(paths.config_file(), "[roster]\nworking_set = [\"quiet\"]\n").unwrap();
    assert!(
        ProductConfig::load(&paths)
            .unwrap_err()
            .contains("needs a description")
    );
}

/// Every name a `spawns` list carries is a template file with a card,
/// checked at load like a working-set member, whether or not the
/// working set reaches it.
#[test]
fn spawns_names_must_be_described_template_files() {
    let (_tmp, paths) = home_with_templates(&[
        (
            "lead",
            "account = \"codex\"\nmodel = \"m\"\ndescription = \"leads\"\nspawns = [\"ghost\"]\n",
        ),
        ("quiet", "account = \"codex\"\nmodel = \"m\"\n"),
    ]);
    let error = ProductConfig::load(&paths).unwrap_err();
    assert!(error.contains("template lead spawns ghost, and no template file ghost.toml exists under ~/.plak/agents or .plak/agents"), "{error}");
    fs::write(
        paths.user_agents_dir().join("lead.toml"),
        "account = \"codex\"\nmodel = \"m\"\ndescription = \"leads\"\nspawns = [\"quiet\"]\n",
    )
    .unwrap();
    let error = ProductConfig::load(&paths).unwrap_err();
    assert!(
        error.contains(
            "template quiet is in template lead's spawns and needs a description for its spawn card"
        ),
        "{error}"
    );
    fs::write(
        paths.user_agents_dir().join("quiet.toml"),
        "account = \"codex\"\nmodel = \"m\"\ndescription = \"q\"\n",
    )
    .unwrap();
    fs::write(paths.config_file(), "[roster]\nworking_set = [\"lead\"]\n").unwrap();
    let loaded = ProductConfig::load(&paths).unwrap();
    assert_eq!(
        crate::roster_input::spawn_closure(&loaded),
        [
            TemplateName::parse("lead").unwrap(),
            TemplateName::parse("quiet").unwrap()
        ]
    );
}

#[test]
fn bounds_reject_outside_their_own_ceiling() {
    for (key, ceiling) in [
        ("max_depth", CEILING_MAX_DEPTH),
        ("max_children", CEILING_MAX_CHILDREN),
        ("max_nodes", CEILING_MAX_NODES),
        ("max_attempts", CEILING_MAX_ATTEMPTS),
        (
            "max_concurrent_requests_per_account",
            CEILING_REQUESTS_PER_ACCOUNT,
        ),
    ] {
        for bad in ["0", "-1"] {
            assert!(
                ProductConfig::parse(&format!("[orchestration]\n{key} = {bad}\n"))
                    .unwrap_err()
                    .contains(&format!("between 1 and {ceiling}")),
                "{key} = {bad}"
            );
        }
        let above = ceiling + 1;
        assert!(
            ProductConfig::parse(&format!("[orchestration]\n{key} = {above}\n"))
                .unwrap_err()
                .contains(&format!("between 1 and {ceiling}")),
            "{key} = {above} must be a load error, never a silent clamp"
        );
        assert!(
            ProductConfig::parse(&format!("[orchestration]\n{key} = {ceiling}\n")).is_ok(),
            "{key} = {ceiling} is the admitted ceiling"
        );
        let mut assigned = ProductConfig::default_product();
        assert!(
            assigned
                .assign_key(&format!("orchestration.{key}"), &above.to_string())
                .is_err(),
            "orchestration.{key} = {above} must be a set error"
        );
    }
    let defaults = ProductConfig::default_product();
    assert_eq!(defaults.max_depth, 1);
    assert_eq!(defaults.max_children, 8);
    assert_eq!(defaults.max_nodes, 32);
}

#[test]
fn max_attempts_and_require_review_evidence_are_orchestration_keys() {
    let defaults = ProductConfig::default_product();
    assert_eq!(defaults.max_attempts, 64);
    assert!(!defaults.require_review_evidence);
    let parsed = ProductConfig::parse(
        "[orchestration]\nmax_attempts = 12\nrequire_review_evidence = true\n",
    )
    .unwrap();
    assert_eq!(parsed.max_attempts, 12);
    assert!(parsed.require_review_evidence);
    assert!(
        ProductConfig::parse("[orchestration]\nmax_attempts = 0\n")
            .unwrap_err()
            .contains("between 1 and")
    );
    // Above the ceiling is a load error now; the snapshot clamp stays on
    // as defense in depth for a value written around the parser.
    let mut raised = ProductConfig::default_product();
    raised.max_attempts = 900;
    let snapshot = crate::roster_input::snapshot_from_config(&raised, "codex", "gpt", None);
    assert_eq!(usize::from(snapshot.max_attempts), MAX_TREE_ATTEMPTS);
    let rendered = parsed.render();
    assert!(rendered.contains("max_attempts = 12\n"));
    assert!(rendered.contains("require_review_evidence = true\n"));
    let reparsed = ProductConfig::parse(&rendered).unwrap();
    assert_eq!(reparsed.max_attempts, 12);
    assert!(reparsed.require_review_evidence);
    let mut assigned = ProductConfig::default_product();
    assigned
        .assign_key("orchestration.max_attempts", "7")
        .unwrap();
    assigned
        .assign_key("orchestration.require_review_evidence", "true")
        .unwrap();
    assert_eq!(assigned.max_attempts, 7);
    assert!(assigned.require_review_evidence);
}

#[test]
fn child_wake_policy_defaults_true_and_round_trips_as_a_home_key() {
    let mut config = ProductConfig::default_product();
    assert!(config.auto_continue);
    config.assign_key("turn.auto_continue", "false").unwrap();
    assert!(!config.auto_continue);
    assert!(!config.session_policy().auto_continue);
    let parsed = ProductConfig::parse(&config.render_annotated()).unwrap();
    assert!(!parsed.auto_continue);
    assert!(ConfigField::AutoContinue.is_home_only());
    assert_eq!(
        config_key("turn.auto_continue").unwrap().scope,
        ConfigScope::Immediate
    );
    assert!(ProductConfig::parse("[turn]\nauto_continue = 1\n").is_err());
}

#[test]
fn auto_continue_is_immediate_home_only_and_rejects_obsolete_key() {
    let mut config = ProductConfig::default_product();
    config.assign_key("turn.auto_continue", "false").unwrap();
    assert!(!config.session_policy().auto_continue);
    assert!(ConfigField::AutoContinue.is_home_only());
    assert_eq!(
        config_key("turn.auto_continue").unwrap().scope,
        ConfigScope::Immediate
    );
    assert!(ProductConfig::parse("[orchestration]\nwake_on_child_result = false\n").is_err());
}

/// The four templates `configs/reference/agents/` ships: the approved
/// Codex provider preset, annotated by the renderer.
fn reference_templates() -> Vec<(String, AgentTemplate)> {
    reference_config().templates.into_iter().collect()
}

/// The reference home is the approved Codex provider preset itself.
fn reference_config() -> ProductConfig {
    ProductConfig::load(&PlakPaths::explicit(presets_dir().join("codex")))
        .expect("the Codex provider preset is the reference example")
}

/// `configs/reference/**`, relative path and exact body.
fn reference_files() -> Vec<(String, String)> {
    let config = reference_config();
    let mut files = vec![("config.toml".to_owned(), config.render_annotated())];
    for (name, template) in reference_templates() {
        files.push((
            format!("agents/{name}.toml"),
            render_template_annotated(&name, &template),
        ));
    }
    files
}

fn repo_configs_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../configs")
}

/// The built-in profile homes the binary embeds (`presets/<provider>`).
fn presets_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("presets")
}

/// The shipped reference tree is this renderer's own output, so it cannot
/// describe a key the parser does not have. Regenerate after a schema
/// change with
/// `PLAK_REGENERATE_REFERENCE_CONFIGS=1 cargo test -p plak reference`.
#[test]
fn the_reference_config_tree_is_the_renderers_own_output() {
    let dir = repo_configs_dir().join("reference");
    let regenerate = std::env::var_os("PLAK_REGENERATE_REFERENCE_CONFIGS").is_some();
    for (relative, body) in reference_files() {
        let path = dir.join(&relative);
        if regenerate {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, body.as_bytes()).unwrap();
            continue;
        }
        let on_disk = fs::read_to_string(&path).unwrap_or_else(|error| {
            panic!(
                "{}: {error}; regenerate with PLAK_REGENERATE_REFERENCE_CONFIGS=1 cargo test -p plak reference",
                path.display()
            )
        });
        assert_eq!(
            on_disk,
            body,
            "{} drifted from the renderer; regenerate with PLAK_REGENERATE_REFERENCE_CONFIGS=1 cargo test -p plak reference",
            path.display()
        );
    }
}

/// Every shipped preset loads as a `PLAK_HOME` with the default role set
/// (root, worker, reviewer, researcher), and each role carries its
/// approved binding.
#[test]
fn the_shipped_presets_load_as_a_home() {
    // The approved (account, model, effort, fast) assignment per role, in
    // the role order root-agent, worker, reviewer, researcher, and the
    // researcher's tools.
    type Row = [(&'static str, &'static str, &'static str, bool); 4];
    // The reference tree is the annotated render of `presets/codex`.
    let provider_codex: Row = [
        ("codex", "gpt-6-astra", "medium", false),
        ("codex", "gpt-6-sol", "medium", false),
        ("codex", "gpt-6-astra", "medium", false),
        ("codex", "gpt-6-luna", "xhigh", true),
    ];
    let codex_tools: &[&str] = &["web_search", "fetch_url"];
    let grok_tools: &[&str] = &["web_search", "x_search", "fetch_url"];
    let expectations: [(&str, Option<&str>, Row, &[&str]); 5] = [
        ("reference", None, provider_codex, codex_tools),
        ("codex", Some("codex"), provider_codex, codex_tools),
        (
            "claude",
            Some("claude"),
            [
                ("claude", "claude-opus-5-5", "high", false),
                ("claude", "claude-opus-5-5", "medium", false),
                ("claude", "claude-opus-5-5", "xhigh", false),
                ("claude", "claude-opus-5-5", "low", false),
            ],
            &["web_search", "web_fetch", "fetch_url"],
        ),
        (
            "grok",
            Some("grok"),
            [
                ("grok", "grok-4.7", "high", false),
                ("grok", "grok-4.7-build-fast", "medium", false),
                ("grok", "grok-4.7", "high", false),
                ("grok", "grok-4.7-build-fast", "medium", false),
            ],
            grok_tools,
        ),
        (
            "multi",
            None,
            [
                ("claude", "claude-opus-5-5", "high", false),
                ("codex", "gpt-6.1-sol", "medium", true),
                ("claude", "claude-opus-5-5", "high", false),
                ("grok", "grok-4.7-build-fast", "medium", false),
            ],
            grok_tools,
        ),
    ];
    for (preset, provider, rows, researcher_tools) in expectations {
        let dir = if preset == "reference" {
            repo_configs_dir().join(preset)
        } else {
            presets_dir().join(preset)
        };
        let paths =
            PlakPaths::resolve(Some(std::ffi::OsStr::new(&dir)), None).expect("preset home");
        let loaded =
            ProductConfig::load(&paths).unwrap_or_else(|error| panic!("{preset}: {error}"));
        assert_eq!(
            loaded.root_agent.as_deref(),
            Some("root-agent"),
            "{preset} must bind root-agent"
        );
        assert_eq!(
            loaded
                .working_set
                .names()
                .iter()
                .map(TemplateName::as_str)
                .collect::<Vec<_>>(),
            ["worker", "reviewer", "researcher"],
            "{preset} working set"
        );
        assert_eq!(loaded.templates.len(), 4, "{preset}");
        assert!(loaded.subagents, "{preset}");
        assert_eq!(loaded.max_depth, 1, "{preset}");
        assert_eq!(loaded.max_children, 8, "{preset}");
        for (name, template) in &loaded.templates {
            assert_eq!(template.delegation, Delegation::Explicit, "{preset}/{name}");
            assert!(!template.may_adopt, "{preset}/{name} must not adopt");
            if let Some(provider) = provider {
                assert_eq!(
                    template.account, provider,
                    "{preset}/{name} must bind the preset's own provider"
                );
            }
        }
        // (role, access, spawns, tools)
        let shape: [(&str, Tier, &[&str], &[&str]); 4] = [
            ("root-agent", Tier::Mutate, &[], &[]),
            ("worker", Tier::Mutate, &[], &[]),
            ("reviewer", Tier::Execute, &[], &[]),
            ("researcher", Tier::Mutate, &[], researcher_tools),
        ];
        for ((role, access, spawns, tools), (account, model, effort, fast)) in
            shape.into_iter().zip(rows.iter())
        {
            let template = &loaded.templates[role];
            assert_eq!(template.account, *account, "{preset}/{role} account");
            assert_eq!(template.model, *model, "{preset}/{role} model");
            assert_eq!(
                template.effort.as_deref(),
                Some(*effort),
                "{preset}/{role} effort"
            );
            assert_eq!(template.is_fast(), *fast, "{preset}/{role} fast_mode");
            assert_eq!(template.max_tier, access, "{preset}/{role} access");
            assert_eq!(
                template
                    .spawns
                    .iter()
                    .map(TemplateName::as_str)
                    .collect::<Vec<_>>(),
                spawns,
                "{preset}/{role} spawns"
            );
            assert_eq!(
                template
                    .tools
                    .entries()
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>(),
                tools,
                "{preset}/{role} tools"
            );
        }
    }
    // The reference tree round-trips: what it renders is what it parses.
    let dir = repo_configs_dir().join("reference");
    let paths = PlakPaths::resolve(Some(std::ffi::OsStr::new(&dir)), None).expect("home");
    assert_eq!(ProductConfig::load(&paths).unwrap(), reference_config());
    // The schema default stays unbound: an empty home picks no provider.
    assert!(ProductConfig::default_product().root_agent.is_none());
}

/// `docs/spec/07-config-reference.md` §3 is canon for the key list, so the
/// table and the spec table are the same set, row for row: same type, same
/// default, same constraint, same sentence.
#[test]
fn the_key_table_is_the_spec_table() {
    let spec = fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/spec/07-config-reference.md"),
    )
    .expect("spec 07");
    let mut documented: Vec<(String, String, String, String, String)> = Vec::new();
    for row in CONFIG_KEYS {
        let constraint = if row.constraint.is_empty() {
            "—".to_owned()
        } else {
            row.constraint.to_owned()
        };
        assert!(row.doc.ends_with('.'), "{} has no sentence", row.path);
        assert!(!row.example.is_empty(), "{} has no example", row.path);
        documented.push((
            row.path.to_owned(),
            row.kind.to_owned(),
            row.default_value(),
            constraint,
            row.doc.to_owned(),
        ));
    }
    let mut once = BTreeSet::new();
    for row in &documented {
        assert!(once.insert(row.0.clone()), "{} is documented twice", row.0);
    }
    // Section 3's table, which the section-4 template table follows; a row
    // is `| `key` | type | `default` | constraint | meaning |`.
    let section = spec
        .split("## 3. `config.toml`")
        .nth(1)
        .and_then(|rest| rest.split("### 3.1").next())
        .expect("spec 07 §3");
    let mut canon: Vec<(String, String, String, String, String)> = Vec::new();
    for line in section.lines() {
        let Some(row) = line.strip_prefix("| `") else {
            continue;
        };
        let cells: Vec<String> = row
            .trim_end_matches('|')
            .split(" | ")
            .map(|cell| cell.trim().replace("\\|", "|").replace('`', ""))
            .collect();
        assert_eq!(cells.len(), 5, "spec 07 §3 row is not five cells: {line}");
        canon.push((
            cells[0].clone(),
            cells[1].clone(),
            cells[2].clone(),
            cells[3].clone(),
            cells[4].clone(),
        ));
    }
    assert_eq!(
        documented, canon,
        "the ConfigKey table and docs/spec/07-config-reference.md §3 disagree"
    );
}

/// `docs/spec/07-config-reference.md` §4 is the template schema in prose,
/// so the field table and the spec table are the same rows, cell for cell.
/// §3 has had this guard since it was written; §4 drifted without one —
/// its `composition` row named "API accounts" where the code named the
/// four of them.
#[test]
fn the_template_field_table_is_the_spec_table() {
    let spec = fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/spec/07-config-reference.md"),
    )
    .expect("spec 07");
    let documented: Vec<(String, String, String, String, String)> = TEMPLATE_FIELD_DOCS
        .iter()
        .map(|row| {
            let constraint = if row.constraint.is_empty() {
                "—".to_owned()
            } else {
                row.constraint.to_owned()
            };
            (
                row.key.to_owned(),
                row.value_type.to_owned(),
                row.default.to_owned(),
                constraint,
                row.meaning.to_owned(),
            )
        })
        .collect();
    let section = spec
        .split("## 4. `agents/<name>.toml`")
        .nth(1)
        .and_then(|rest| rest.split("`account` and `model` are required").next())
        .expect("spec 07 §4");
    let mut canon: Vec<(String, String, String, String, String)> = Vec::new();
    for line in section.lines() {
        let Some(row) = line.strip_prefix("| `") else {
            continue;
        };
        let cells: Vec<String> = row
            .trim_end_matches('|')
            .split(" | ")
            .map(|cell| cell.trim().replace("\\|", "|").replace('`', ""))
            .collect();
        assert_eq!(cells.len(), 5, "spec 07 §4 row is not five cells: {line}");
        canon.push((
            cells[0].clone(),
            cells[1].clone(),
            cells[2].clone(),
            cells[3].clone(),
            cells[4].clone(),
        ));
    }
    assert_eq!(
        documented, canon,
        "the TemplateFieldDoc table and docs/spec/07-config-reference.md §4 disagree"
    );
}

/// The four `[orchestration]` bounds spell their ceiling as a literal.
/// Raising a ceiling in code must not leave the reference file and §3
/// promising the old number.
#[test]
fn every_bound_constraint_names_its_own_ceiling() {
    for (path, ceiling) in [
        ("orchestration.max_depth", CEILING_MAX_DEPTH),
        ("orchestration.max_children", CEILING_MAX_CHILDREN),
        ("orchestration.max_nodes", CEILING_MAX_NODES),
        ("orchestration.max_attempts", CEILING_MAX_ATTEMPTS),
        (
            "orchestration.max_concurrent_requests_per_account",
            CEILING_REQUESTS_PER_ACCOUNT,
        ),
    ] {
        let row = config_key(path).expect(path);
        assert!(
            row.constraint.contains(&format!("1..={ceiling}")),
            "{path} constraint does not name its ceiling {ceiling}: {}",
            row.constraint
        );
    }
}

/// The template comment map is the template schema: every field a fully
/// populated template renders is documented, and nothing else is.
#[test]
fn every_template_field_is_documented_once() {
    let populated = AgentTemplate {
        account: "codex".to_owned(),
        model: "gpt-5.6-luna".to_owned(),
        effort: Some("high".to_owned()),
        extra: Some("priority".to_owned()),
        composition: Some(ProductCompositionSelection::NativePtc),
        protocol: Some(ProductProtocolSelection::Auto),
        description: "does one thing".to_owned(),
        max_tier: Tier::Mutate,
        tools: TemplateToolSelection::parse(["web_search", "mcp:github/create_issue"]).unwrap(),
        spawns: Vec::new(),
        may_adopt: false,
        delegation: Delegation::Proactive,
        cost_tier: Some(2),
        max_turns: Some(24),
        on_mint: Some("cargo fetch".to_owned()),
        show_model: true,
        env: BTreeMap::from([("KEY".to_owned(), "value".to_owned())]),
    };
    let rendered = render_template(&populated);
    let mut documented = BTreeSet::new();
    for doc in TEMPLATE_FIELD_DOCS {
        assert!(
            documented.insert(doc.key),
            "{} is documented twice",
            doc.key
        );
        assert!(doc.meaning.ends_with('.'), "{} has no sentence", doc.key);
        assert!(
            rendered.contains(&format!("{} = ", doc.key)) || doc.key == "env",
            "{} is documented but never rendered",
            doc.key
        );
    }
    for line in rendered.lines() {
        let Some((key, _)) = line.split_once(" = ") else {
            continue;
        };
        assert!(
            TEMPLATE_FIELD_DOCS.iter().any(|doc| doc.key == key) || key == "KEY",
            "{key} is rendered but undocumented"
        );
    }
    // The annotated form is still a template file: it parses back.
    let annotated = render_template_annotated("worker", &populated);
    assert_eq!(parse_template(&annotated, "worker").unwrap(), populated);
    // A field the template leaves unset prints as a commented example.
    let bare = AgentTemplate {
        account: "codex".to_owned(),
        model: "gpt-5.6-luna".to_owned(),
        ..AgentTemplate::default()
    };
    let annotated = render_template_annotated("bare", &bare);
    assert!(annotated.contains("# on_mint = \"cargo fetch\"\n"));
    assert!(annotated.contains("# [env]\n"));
    assert_eq!(parse_template(&annotated, "bare").unwrap(), bare);
}

/// A `#` inside a string is data, not the start of a comment. Two removed
/// readers got this wrong in turn — `split('#')` truncated a description
/// at the first `#` and shipped half a spawn card — so the rule stays
/// pinned now that TOML is the one that answers it.
#[test]
fn a_hash_inside_a_quoted_value_is_not_a_comment() {
    let parsed = ProductConfig::parse(
        "[root]\nagent = \"router\" # the genesis template\n[tui]\ntheme = \"default-light\"\n",
    )
    .unwrap();
    assert_eq!(parsed.root_agent.as_deref(), Some("router"));
    assert_eq!(parsed.theme, ProductTheme::DefaultLight);

    let template = parse_template(
        "account = \"codex\"\nmodel = \"gpt-5.6-luna\"\ndescription = \"fixes the #123 bug\" # card\n",
        "fixer",
    )
    .unwrap();
    assert_eq!(template.description, "fixes the #123 bug");
    // A `#` line and a trailing comment outside a string still comment.
    let commented =
        ProductConfig::parse("# leading comment\n[orchestration]\nmax_depth = 2 # lowered\n")
            .unwrap();
    assert_eq!(commented.max_depth, 2);
    // The rendered reference round-trips with its comments attached.
    let annotated = ProductConfig::default_product().render_annotated();
    assert_eq!(
        ProductConfig::parse(&annotated).unwrap(),
        ProductConfig::default_product()
    );
}

/// One key, one meaning: `config set` takes the file's own array form as
/// well as the shell shorthand, so a copy-paste cannot be the wrong
/// grammar.
#[test]
fn working_set_takes_the_file_array_and_the_shell_shorthand() {
    let mut cfg = ProductConfig::default_product();
    cfg.templates
        .insert("one".to_owned(), template("codex", "m"));
    cfg.templates
        .insert("two".to_owned(), template("claude", "n"));
    for spelling in [
        "one,two",
        "one, two",
        "one two",
        "[\"one\", \"two\"]",
        "[\"one\",\"two\"]",
    ] {
        cfg.set_key("roster.working_set", spelling).unwrap();
        assert_eq!(
            cfg.working_set
                .names()
                .iter()
                .map(TemplateName::as_str)
                .collect::<Vec<_>>(),
            ["one", "two"],
            "{spelling}"
        );
    }
    // The file grammar is the array only. A bare `one, two` is not a TOML
    // value at all, so the document layer names the line it broke on, and
    // a value of the wrong TOML type gets the named array error.
    let bare = ProductConfig::parse("[roster]\nworking_set = one, two\n").unwrap_err();
    assert!(bare.contains("line 2"), "{bare}");
    assert!(
        ProductConfig::parse("[roster]\nworking_set = 3\n")
            .unwrap_err()
            .contains("must be an array")
    );
    assert!(
        ProductConfig::parse("[roster]\nworking_set = [\"one\", 2]\n")
            .unwrap_err()
            .contains("must be an array")
    );
    assert!(
        cfg.set_key("roster.working_set", "[\"one\", two]")
            .unwrap_err()
            .contains("must be an array")
    );
    cfg.set_key("roster.working_set", "").unwrap();
    assert!(cfg.working_set.is_empty());
}

#[test]
fn removed_bare_config_set_aliases_name_dotted_keys() {
    let mut cfg = ProductConfig::default_product();
    for (bare, dotted, value) in [
        ("mouse", "tui.mouse", "false"),
        ("theme", "tui.theme", "catppuccin-mocha"),
        ("compaction_enabled", "compaction.enabled", "false"),
    ] {
        let mut bare_cfg = ProductConfig::default_product();
        assert!(bare_cfg.set_key(bare, value).unwrap_err().contains(dotted));
        let mut dotted_cfg = ProductConfig::default_product();
        dotted_cfg.set_key(dotted, value).unwrap();
        assert_ne!(bare_cfg, dotted_cfg, "{bare} must not pin {dotted}");
    }
    for bare in [
        "scroll_lines",
        "show_thinking",
        "group_tool_calls",
        "reduced_motion",
        "working_set",
        "agent",
        "subagents",
        "max_depth",
        "max_bytes",
        "enabled",
    ] {
        assert_eq!(
            cfg.set_key(bare, "1").unwrap_err(),
            format!("unknown config key: {bare}"),
            "{bare} must not have a bare spelling"
        );
    }
}

/// `max_agents_per_cell` is internal now: the default still feeds the
/// cell admission bound, and a value written around the parser is still
/// clipped where the budget is published.
#[test]
fn max_agents_per_cell_is_internal_and_still_bounded() {
    use plakous_hosted::cell_budget::DEFAULT_MAX_AGENTS_PER_CELL;
    assert_eq!(
        ProductConfig::default_product().max_agents_per_cell,
        DEFAULT_MAX_AGENTS_PER_CELL
    );
    let mut raised = ProductConfig::default_product();
    raised.max_agents_per_cell = 4096;
    let snapshot = crate::roster_input::snapshot_from_config(&raised, "codex", "gpt", None);
    assert_eq!(
        usize::from(snapshot.max_agents_per_cell),
        usize::from(plakous_hosted::cell_budget::CEILING_MAX_AGENTS_PER_CELL)
    );
}

#[test]
fn an_absent_config_file_is_the_schema_default_for_the_topology_gate() {
    let (_tmp, paths) = PlakPaths::isolated_test_home();
    paths.ensure_layout().unwrap();
    assert!(!paths.config_file().is_file());
    assert!(SessionPolicy::load(&paths).unwrap().topology_enabled);
}

#[test]
fn an_unreadable_config_file_refuses_session_policy() {
    let (_tmp, paths) = PlakPaths::isolated_test_home();
    paths.ensure_layout().unwrap();
    fs::write(paths.config_file(), "[orchestration\nsubagents = true\n").unwrap();
    assert_eq!(
        SessionPolicy::load(&paths).unwrap_err(),
        ProductConfig::load(&paths).unwrap_err()
    );
}

#[test]
fn session_policy_refuses_an_unreadable_config_with_its_named_error() {
    let (_tmp, paths) = PlakPaths::isolated_test_home();
    paths.ensure_layout().unwrap();
    fs::write(paths.config_file(), "[tui]\ncollapsed_edit_blocks = true\n").unwrap();
    let result = SessionPolicy::load(&paths);
    assert!(result.unwrap_err().contains("collapsed_edit_blocks"));
}

#[test]
fn an_unparseable_template_refuses_session_policy() {
    let (_tmp, paths) = home_with_templates(&[("worker", "account = \"codex\"\nmodel =\n")]);
    assert!(ProductConfig::load(&paths).is_err());
    assert!(
        SessionPolicy::load(&paths).is_err(),
        "an unparseable agents/*.toml is an unreadable roster"
    );
}

#[test]
fn subagents_true_with_an_empty_working_set_is_the_legal_default() {
    let cfg = ProductConfig::default_product();
    assert!(cfg.subagents);
    assert!(cfg.working_set.is_empty());
    assert!(cfg.templates.is_empty());
    assert_eq!(cfg.occupancy, WriterMode::SingleLease);
}

// The closed `plak config set` key list.

#[test]
fn config_set_accepts_exactly_the_closed_key_list() {
    let mut cfg = ProductConfig::default_product();
    cfg.templates
        .insert("one".to_owned(), template("codex", "m"));
    cfg.templates
        .insert("two".to_owned(), template("claude", "n"));
    for (key, value) in [
        ("root.agent", "one"),
        ("roster.working_set", "one, two"),
        ("roster.working_set", "two"),
        ("orchestration.subagents", "false"),
        ("orchestration.max_depth", "2"),
        ("orchestration.max_children", "4"),
        ("orchestration.max_nodes", "16"),
        ("tui.mouse", "false"),
        ("tui.theme", "catppuccin-mocha"),
        ("compaction.enabled", "false"),
        ("telemetry.max_bytes", "1000"),
        ("telemetry.max_age_days", "7"),
    ] {
        cfg.set_key(key, value)
            .unwrap_or_else(|error| panic!("{key}: {error}"));
    }
    assert_eq!(cfg.root_agent.as_deref(), Some("one"));
    assert_eq!(cfg.working_set.names().len(), 1);
    assert!(cfg.working_set.contains("two"));
    assert_eq!(cfg.max_children, 4);
    cfg.set_key("roster.working_set", "").unwrap();
    assert!(cfg.working_set.is_empty());

    for key in [
        "account",
        "model",
        "effort",
        "extra",
        "composition",
        "protocol",
        "subagents",
        "max_agents",
        "child_may_spawn",
        "default.account",
        "default.model",
        "default.subagents",
        "default_account",
        "roster.review",
        "roster.review.account",
        "orchestration.child_may_spawn",
        "not.a.key",
    ] {
        assert_eq!(
            cfg.set_key(key, "codex").unwrap_err(),
            format!("unknown config key: {key}"),
            "{key} must not be settable"
        );
    }
    // templates are never authored through `plak config set`.
    assert_eq!(
        cfg.set_key("agent.one.account", "codex").unwrap_err(),
        "templates are file-authored; edit ~/.plak/agents/one.toml"
    );
}

#[test]
fn config_set_validates_against_the_same_rule_set_as_load() {
    let mut cfg = ProductConfig::default_product();
    let mut uncarded = template("codex", "m");
    uncarded.description = String::new();
    cfg.templates.insert("uncarded".to_owned(), uncarded);
    assert!(
        cfg.set_key("root.agent", "missing")
            .unwrap_err()
            .contains("names no template file missing.toml")
    );
    assert!(
        cfg.set_key("roster.working_set", "uncarded")
            .unwrap_err()
            .contains("needs a description")
    );
    cfg.templates.get_mut("uncarded").unwrap().description = "m worker".into();
    cfg.set_key("roster.working_set", "uncarded").unwrap();
    cfg.set_key("roster.working_set", "").unwrap();
    assert!(cfg.working_set.is_empty());
}

// Fast x spawn.

#[test]
fn a_fast_template_may_spawn_anywhere() {
    let mut cfg = ProductConfig::default_product();
    let mut fast = template("codex", "gpt-5.6-luna");
    fast.extra = Some("priority".into());
    fast.spawns = vec![TemplateName::parse("leaf").unwrap()];
    fast.may_adopt = true;
    cfg.templates.insert("fast".to_owned(), fast);
    cfg.templates
        .insert("leaf".to_owned(), template("codex", "gpt-5.6-luna"));
    cfg.working_set = WorkingSet::parse(["fast"]).unwrap();
    cfg.validate().unwrap();
    cfg.working_set = WorkingSet::default();
    cfg.root_agent = Some("fast".to_owned());
    cfg.validate().unwrap();
}

#[test]
fn processing_tier_is_derived_from_extra_and_never_guessed() {
    let mut fast = template("codex", "gpt");
    fast.extra = Some("priority".into());
    assert!(matches!(
        fast.processing_tier(),
        ProcessingTierDisposition::ProviderNamed(_)
    ));
    assert_eq!(
        template("codex", "gpt").processing_tier(),
        ProcessingTierDisposition::Standard
    );
}

// Template readiness projection.

#[test]
fn readiness_requires_credential_ready_and_catalog_admission() {
    let sections = catalog(
        "codex",
        true,
        vec![catalog_entry(
            "gpt-5.6-luna",
            &["low", "high"],
            &["priority"],
        )],
    );
    let mut record = template("codex", "gpt-5.6-luna");
    record.effort = Some("low".into());
    record.extra = Some("priority".into());
    assert_eq!(
        template_readiness(&record, CredentialReadiness::Ready, &sections),
        TemplateReadiness::Ready
    );

    for (credential, expected) in [
        (
            CredentialReadiness::NotLoggedIn,
            TemplateNotReady::NotLoggedIn,
        ),
        (
            CredentialReadiness::Expired,
            TemplateNotReady::CredentialExpired,
        ),
        (CredentialReadiness::NoApiKey, TemplateNotReady::NoApiKey),
    ] {
        assert_eq!(
            template_readiness(&record, credential, &sections),
            TemplateReadiness::NotReady(expected)
        );
    }

    let mut absent = record.clone();
    absent.model = "gpt-absent".into();
    assert_eq!(
        template_readiness(&absent, CredentialReadiness::Ready, &sections),
        TemplateReadiness::NotReady(TemplateNotReady::ModelNotAdmitted)
    );

    let mut effort = record.clone();
    effort.effort = Some("xhigh".into());
    assert_eq!(
        template_readiness(&effort, CredentialReadiness::Ready, &sections),
        TemplateReadiness::NotReady(TemplateNotReady::EffortNotAdmitted)
    );

    let standard_only = catalog(
        "codex",
        true,
        vec![catalog_entry("gpt-5.6-luna", &["low", "high"], &[])],
    );
    assert_eq!(
        template_readiness(&record, CredentialReadiness::Ready, &standard_only),
        TemplateReadiness::NotReady(TemplateNotReady::TierNotAdvertised)
    );

    // The projection is pure: the same pair answers the same way.
    assert_eq!(
        template_readiness(&record, CredentialReadiness::Ready, &sections),
        template_readiness(&record, CredentialReadiness::Ready, &sections)
    );
}

#[test]
fn credential_readiness_is_derived_from_the_ready_catalog() {
    let sections = catalog("codex", false, Vec::new());
    assert_eq!(
        catalog_credential_readiness("codex", &sections),
        CredentialReadiness::NotLoggedIn
    );
    assert_eq!(
        catalog_credential_readiness("openrouter", &sections),
        CredentialReadiness::NoApiKey
    );
    let ready = catalog("codex", true, vec![catalog_entry("m", &[], &[])]);
    assert_eq!(
        catalog_credential_readiness("codex", &ready),
        CredentialReadiness::Ready
    );
    for reason in [
        TemplateNotReady::NotLoggedIn,
        TemplateNotReady::CredentialExpired,
        TemplateNotReady::NoApiKey,
        TemplateNotReady::ModelNotAdmitted,
        TemplateNotReady::EffortNotAdmitted,
        TemplateNotReady::TierNotAdvertised,
    ] {
        assert_ne!(reason.reason(), "");
        assert!(!reason.reason().contains("sk-"));
    }
}

#[test]
fn a_not_ready_working_set_template_keeps_its_place_and_is_still_projected() {
    let (_tmp, paths) =
        home_with_templates(&[("reviewer", &template_body("claude", "claude-opus-5", ""))]);
    fs::write(
        paths.config_file(),
        "[roster]\nworking_set = [\"reviewer\"]\n",
    )
    .unwrap();
    let cfg = ProductConfig::load(&paths).unwrap();
    let sections = catalog("claude", false, Vec::new());
    let rows = cfg.readiness_rows(&sections);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].name, "reviewer");
    assert_eq!(
        rows[0].readiness,
        TemplateReadiness::NotReady(TemplateNotReady::NotLoggedIn)
    );
    assert!(!rows[0].readiness.is_ready());
    assert_eq!(rows[0].readiness.reason(), Some("not logged in"));
    // The working-set entry survives its template becoming NotReady.
    assert!(cfg.working_set.contains("reviewer"));
    assert!(cfg.working_set_template("reviewer").is_some());
    assert!(cfg.working_set_template("other").is_none());
}

// Pre-attach shell binding and the root accessors.

#[test]
fn root_binding_mints_a_default_template_file_and_clearing_keeps_it() {
    let mut cfg = ProductConfig::default_product();
    assert_eq!(cfg.default_account(), None);
    cfg.bind_root_tuple("grok", "grok-4.6", Some("low"));
    assert_eq!(cfg.root_agent.as_deref(), Some(MINTED_ROOT_TEMPLATE_NAME));
    assert_eq!(cfg.default_account(), Some("grok"));
    assert_eq!(cfg.default_model(), Some("grok-4.6"));
    assert_eq!(cfg.default_effort(), Some("low"));
    cfg.validate().unwrap();
    let (_tmp, paths) = PlakPaths::isolated_test_home();
    paths.ensure_layout().unwrap();
    cfg.save(&paths).unwrap();
    assert!(paths.user_agents_dir().join("default.toml").is_file());
    let reopened = ProductConfig::load(&paths).unwrap();
    assert_eq!(reopened, cfg);
    assert!(
        !fs::read_to_string(paths.config_file())
            .unwrap()
            .contains("[agent."),
        "config.toml carries no template body"
    );

    // Clearing drops the pointer only; the template file is the user's.
    cfg.clear_root_binding();
    assert_eq!(cfg.root_agent, None);
    assert!(cfg.templates.contains_key(MINTED_ROOT_TEMPLATE_NAME));
    cfg.save(&paths).unwrap();
    assert!(paths.user_agents_dir().join("default.toml").is_file());
    assert_eq!(ProductConfig::load(&paths).unwrap().root_agent, None);
}

/// A pre-attach shell binding never rewrites a working-set template body.
///
/// Binding the root tuple in place would silently re-author the spawnable
/// construction the next epoch will use, bypassing the successor-epoch apply
/// gate. The tuple is redirected into a freshly minted template instead.
#[test]
fn a_shell_binding_never_rewrites_a_working_set_template() {
    let mut cfg = ProductConfig::default_product();
    cfg.templates
        .insert("primary".to_owned(), template("codex", "gpt-5.6-luna"));
    cfg.root_agent = Some("primary".to_owned());
    cfg.working_set = WorkingSet::parse(["primary"]).unwrap();

    cfg.bind_root_tuple("grok", "grok-4.6", Some("low"));

    // The working set keeps the construction it named.
    assert!(cfg.working_set.contains("primary"));
    let pinned = cfg.templates.get("primary").unwrap();
    assert_eq!(pinned.account, "codex");
    assert_eq!(pinned.model, "gpt-5.6-luna");
    // The root points at a distinct minted template carrying the new tuple.
    assert_eq!(cfg.root_agent.as_deref(), Some(MINTED_ROOT_TEMPLATE_NAME));
    assert_eq!(cfg.default_account(), Some("grok"));
    assert_eq!(cfg.default_model(), Some("grok-4.6"));
    cfg.validate().unwrap();

    // With `default` itself in the working set, the mint takes the next name.
    let mut taken = ProductConfig::default_product();
    taken
        .templates
        .insert("default".to_owned(), template("codex", "gpt-5.6-luna"));
    taken.root_agent = Some("default".to_owned());
    taken.working_set = WorkingSet::parse(["default"]).unwrap();
    taken.bind_root_tuple("grok", "grok-4.6", None);
    assert_eq!(taken.root_agent.as_deref(), Some("default-2"));
    assert_eq!(taken.templates.get("default").unwrap().account, "codex");
    taken.validate().unwrap();
}

/// `root.agent ""` clears the pointer, as
/// `roster.working_set ""` empties the set. There is otherwise no way to
/// unset `[root] agent`.
#[test]
fn an_empty_root_agent_clears_the_root_pointer() {
    let mut cfg = ProductConfig::default_product();
    cfg.bind_root_tuple("grok", "grok-4.6", None);
    cfg.set_key("root.agent", "").unwrap();
    assert_eq!(cfg.root_agent, None);

    let mut named = ProductConfig::default_product();
    named
        .templates
        .insert("primary".to_owned(), template("codex", "m"));
    named.root_agent = Some("primary".to_owned());
    named.set_key("root.agent", "").unwrap();
    assert_eq!(named.root_agent, None);
    assert!(named.templates.contains_key("primary"));
}

#[test]
fn composition_for_tuple_is_bound_to_the_exact_root_tuple() {
    let mut cfg = ProductConfig::default_product();
    cfg.bind_root_tuple("grok", "grok-4.6", None).composition =
        Some(ProductCompositionSelection::Direct);
    assert_eq!(
        cfg.composition_for_tuple(Some("grok"), Some("grok-4.6")),
        ProductCompositionSelection::Direct
    );
    assert_eq!(
        cfg.composition_for_tuple(Some("grok"), Some("grok-4.5")),
        ProductCompositionSelection::FollowCatalog
    );
    assert_eq!(
        cfg.composition_for_tuple(None, None),
        ProductCompositionSelection::FollowCatalog
    );
}

#[test]
fn themes_parse_render_and_set_key() {
    for (value, expected) in [
        ("default-dark", ProductTheme::DefaultDark),
        ("default-light", ProductTheme::DefaultLight),
        ("catppuccin-latte", ProductTheme::CatppuccinLatte),
        ("catppuccin-frappe", ProductTheme::CatppuccinFrappe),
        ("catppuccin-macchiato", ProductTheme::CatppuccinMacchiato),
        ("catppuccin-mocha", ProductTheme::CatppuccinMocha),
    ] {
        let parsed = ProductConfig::parse(&format!("[tui]\ntheme = \"{value}\"\n")).unwrap();
        assert_eq!(parsed.theme, expected);
        assert!(parsed.render().contains(&format!("theme = \"{value}\"")));

        let mut set = ProductConfig::default_product();
        set.set_key("tui.theme", value).unwrap();
        assert_eq!(set.theme, expected);
        set.set_key("tui.theme", value).unwrap();
        assert_eq!(set.theme, expected);
    }
    for (legacy, expected, canonical) in [
        ("dark", ProductTheme::DefaultDark, "default-dark"),
        ("light", ProductTheme::DefaultLight, "default-light"),
    ] {
        let parsed = ProductConfig::parse(&format!("[tui]\ntheme = \"{legacy}\"\n")).unwrap();
        assert_eq!(parsed.theme, expected);
        assert!(
            parsed
                .render()
                .contains(&format!("theme = \"{canonical}\""))
        );
    }
    assert!(ProductConfig::parse("[tui]\ntheme = \"sepia\"\n").is_err());
    assert!(
        ProductConfig::default_product()
            .set_key("tui.theme", "sepia")
            .is_err()
    );
}

/// The file is TOML, so the whole of TOML's string and array vocabulary
/// reads back as the value it spells.
#[test]
fn toml_strings_arrays_and_escapes_read_back_as_written() {
    let parsed = ProductConfig::parse(
        "[root]\nagent = 'literal-root'\n\
             [roster]\nworking_set = [\n  \"first\",  # trailing\n  'second',\n]\n",
    )
    .unwrap();
    assert_eq!(parsed.root_agent.as_deref(), Some("literal-root"));
    assert_eq!(
        parsed
            .working_set
            .names()
            .iter()
            .map(TemplateName::as_str)
            .collect::<Vec<_>>(),
        ["first", "second"]
    );

    let template = parse_template(
        "account = \"codex\"\nmodel = \"gpt-5.6-luna\"\n\
             description = \"a \\\"quoted\\\" card — Ünicode ✓, tab\\tand \\\\ backslash\"\n\
             on_mint = \"\"\"\nline one\nline two\"\"\"\n\
             [env]\nGREETING = \"héllo\"\n",
        "t",
    )
    .unwrap();
    assert_eq!(
        template.description,
        "a \"quoted\" card — Ünicode ✓, tab\tand \\ backslash"
    );
    assert_eq!(template.on_mint.as_deref(), Some("line one\nline two"));
    assert_eq!(
        template.env.get("GREETING").map(String::as_str),
        Some("héllo")
    );
    // The renderer escapes what it writes, so a card carrying a quote is
    // still the same card after a round trip through a file.
    assert_eq!(
        parse_template(&render_template(&template), "t").unwrap(),
        template
    );
}

/// A duplicate key is a refusal, not a last-one-wins overwrite.
/// A key written above the first section header has no section to belong
/// to, and says so without an empty one in the message.
#[test]
fn a_key_outside_every_section_is_a_named_unknown_key() {
    assert_eq!(
        ProductConfig::parse("theme = \"default-dark\"\n").unwrap_err(),
        "unknown config key: theme (line 1, column 1)"
    );
}

#[test]
fn duplicate_keys_and_sections_are_refused() {
    for text in [
        "[tui]\ntheme = \"default-dark\"\ntheme = \"default-light\"\n",
        "[tui]\ntheme = \"default-dark\"\n[tui]\nmouse = false\n",
    ] {
        let error = ProductConfig::parse(text).unwrap_err();
        assert!(error.contains("duplicate key"), "{error}");
        assert!(error.contains("line 3"), "{error}");
    }
}

/// A value of the wrong TOML type names its key and where it is.
#[test]
fn wrong_types_name_the_key_and_the_location() {
    for (text, needle) in [
        ("[root]\nagent = 5\n", "[root] agent must be a string"),
        ("[tui]\ntheme = true\n", "[tui] theme must be a string"),
        (
            "[tui]\nmouse = \"yes\"\n",
            "boolean config values must be true or false",
        ),
        (
            "[orchestration]\nmax_depth = \"2\"\n",
            "[orchestration] max_depth must be an integer between 1 and 4",
        ),
        (
            "[telemetry]\nmax_bytes = true\n",
            "telemetry.max_bytes must be a positive integer",
        ),
        (
            "[tui]\nscroll_lines = \"3\"\n",
            "[tui] scroll_lines must be an integer between",
        ),
        (
            "[turn]\nmax_model_rounds = 1025\n",
            "[turn] max_model_rounds must be an integer between 1 and 1024",
        ),
        (
            "[turn]\nmax_model_rounds = 0\n",
            "[turn] max_model_rounds must be an integer between 1 and 1024",
        ),
    ] {
        let error = ProductConfig::parse(text).unwrap_err();
        assert!(error.contains(needle), "{text} gave {error}");
        assert!(error.contains("line 2, column"), "{text} gave {error}");
    }
}

/// `[turn] max_model_rounds` reaches the session policy every agent is
/// built from, and `plak config set` takes the same range as the file.
#[test]
fn the_turn_round_cap_reaches_the_session_policy() {
    assert_eq!(
        SessionPolicy::defaults().max_model_rounds,
        crate::product_agent::NATIVE_RUN_MAX_MODEL_ROUNDS
    );
    let parsed = ProductConfig::parse("[turn]\nmax_model_rounds = 64\n").unwrap();
    assert_eq!(parsed.session_policy().max_model_rounds, 64);
    let mut config = ProductConfig::default_product();
    config.set_key("turn.max_model_rounds", "1024").unwrap();
    assert_eq!(config.session_policy().max_model_rounds, 1_024);
    assert!(config.set_key("turn.max_model_rounds", "1025").is_err());
}

/// Booleans are TOML booleans. `1`, `yes`, `0` and `no` were accepted by
/// the removed grammar and are not values any more, from a file or from
/// the command line.
#[test]
fn booleans_are_real_toml_booleans_only() {
    assert!(
        ProductConfig::parse("[tui]\nmouse = true\n")
            .unwrap()
            .tui_mouse
    );
    assert!(
        !ProductConfig::parse("[tui]\nmouse = false\n")
            .unwrap()
            .tui_mouse
    );
    for rejected in ["1", "yes", "0", "no", "True", "\"true\""] {
        let text = format!("[tui]\nmouse = {rejected}\n");
        assert!(ProductConfig::parse(&text).is_err(), "{rejected}");
        assert!(
            ProductConfig::default_product()
                .set_key("tui.mouse", rejected)
                .is_err(),
            "{rejected}"
        );
    }
    ProductConfig::default_product()
        .set_key("tui.mouse", "false")
        .unwrap();
}

/// A home whose `config.toml` a person wrote, with its comments and its
/// own spacing.
fn hand_written_home() -> (tempfile::TempDir, PlakPaths, String) {
    let (tmp, paths) = home_with_templates(&[("lead", &template_body("codex", "m", ""))]);
    let text = "# my plak home\n\n\
                    [root]\n\
                    # the genesis template\n\
                    agent = \"lead\"\n\n\
                    [tui]\n\
                    mouse    =   false      # the terminal keeps selection\n\
                    theme = \"catppuccin-mocha\"\n\n\
                    [telemetry]\n\
                    max_age_days = 30\n"
        .to_owned();
    fs::write(paths.config_file(), &text).unwrap();
    (tmp, paths, text)
}

/// A single-key save rewrites that one line and leaves every other byte of
/// the file where the operator put it.
#[test]
fn a_single_key_save_keeps_every_other_byte_of_the_file() {
    let (_tmp, paths, before) = hand_written_home();
    let mut config = ProductConfig::load(&paths).unwrap();
    config.set_key("tui.theme", "catppuccin-latte").unwrap();
    config.save(&paths).unwrap();
    let after = fs::read_to_string(paths.config_file()).unwrap();
    assert_eq!(
        after,
        before.replace(
            "theme = \"catppuccin-mocha\"",
            "theme = \"catppuccin-latte\""
        )
    );
    // The comments, the unrelated hand-set keys and the operator's own
    // spacing all survived, and the file still means what it says.
    let reloaded = ProductConfig::load(&paths).unwrap();
    assert_eq!(reloaded.theme, ProductTheme::CatppuccinLatte);
    assert!(!reloaded.tui_mouse);
    assert_eq!(reloaded.telemetry_max_age_days, 30);
    assert_eq!(reloaded.root_agent.as_deref(), Some("lead"));
}

/// An operator may spell a section as an inline table; a save edits it in
/// place for a changed key and for a new key alike, and never panics.
#[test]
fn an_inline_table_section_is_edited_in_place() {
    let (_tmp, paths, _) = hand_written_home();
    fs::write(
        paths.config_file(),
        "# inline\ntui = { mouse = false, theme = \"catppuccin-mocha\" }\n[root]\nagent = \"lead\"\n",
    )
    .unwrap();
    let mut config = ProductConfig::load(&paths).unwrap();
    config.set_key("tui.theme", "catppuccin-latte").unwrap();
    config.set_key("tui.scroll_lines", "7").unwrap();
    config.save(&paths).unwrap();
    let after = fs::read_to_string(paths.config_file()).unwrap();
    assert!(after.starts_with("# inline\n"), "{after}");
    assert!(after.contains("mouse = false"), "{after}");
    assert!(after.contains("theme = \"catppuccin-latte\""), "{after}");
    assert!(after.contains("scroll_lines = 7"), "{after}");
    assert!(!after.contains("catppuccin-mocha"), "{after}");
    let reloaded = ProductConfig::load(&paths).unwrap();
    assert_eq!(reloaded.theme, ProductTheme::CatppuccinLatte);
    assert!(!reloaded.tui_mouse);
    assert_eq!(reloaded.scroll_lines, 7);

    // Clearing a key removes it from an inline table too; a stale bind
    // must not resurrect on the next load.
    fs::write(
        paths.config_file(),
        "root = { agent = \"lead\" }\ntui = { mouse = false }\n",
    )
    .unwrap();
    let mut config = ProductConfig::load(&paths).unwrap();
    config.set_key("root.agent", "").unwrap();
    config.save(&paths).unwrap();
    let cleared = fs::read_to_string(paths.config_file()).unwrap();
    assert!(!cleared.contains("agent"), "{cleared}");
    assert!(cleared.contains("mouse = false"), "{cleared}");
    assert_eq!(ProductConfig::load(&paths).unwrap().root_agent, None);
}

/// A key the file does not carry is appended under its own section; a key
/// the config no longer carries is removed.
#[test]
fn a_save_adds_a_missing_key_and_removes_a_cleared_one() {
    let (_tmp, paths, _) = hand_written_home();
    let mut config = ProductConfig::load(&paths).unwrap();
    config.set_key("compaction.enabled", "false").unwrap();
    config.save(&paths).unwrap();
    let added = fs::read_to_string(paths.config_file()).unwrap();
    assert!(added.contains("[compaction]\nenabled = false\n"), "{added}");
    assert!(
        added.contains("# the genesis template\nagent = \"lead\""),
        "{added}"
    );

    let mut config = ProductConfig::load(&paths).unwrap();
    config.set_key("root.agent", "").unwrap();
    config.save(&paths).unwrap();
    let cleared = fs::read_to_string(paths.config_file()).unwrap();
    assert!(!cleared.contains("agent = "), "{cleared}");
    assert!(
        cleared.contains("theme = \"catppuccin-mocha\""),
        "{cleared}"
    );
    assert!(ProductConfig::load(&paths).unwrap().root_agent.is_none());
}

/// A theme alias normalizes on the next save, and the surgical editor is
/// what makes that true: `dark` and `default-dark` are different strings,
/// so the key counts as changed and is rewritten even when the save was
/// about some other key (spec 02 §5.8, spec 07 §3.2).
#[test]
fn a_theme_alias_is_rewritten_canonically_by_any_save() {
    for (alias, canonical) in [("dark", "default-dark"), ("light", "default-light")] {
        let (_tmp, paths) = home_with_templates(&[]);
        let before =
            format!("# hand written\n\n[tui]\ntheme = \"{alias}\"\nmouse    =   false   # kept\n");
        fs::write(paths.config_file(), &before).unwrap();

        // A save of an unrelated key still normalizes the alias.
        let mut config = ProductConfig::load(&paths).unwrap();
        config.set_key("tui.scroll_lines", "7").unwrap();
        config.save(&paths).unwrap();

        let after = fs::read_to_string(paths.config_file()).unwrap();
        assert!(
            after.contains(&format!("theme = \"{canonical}\"")),
            "{after}"
        );
        assert!(!after.contains(&format!("\"{alias}\"")), "{after}");
        // Everything the operator wrote around it is still theirs.
        assert!(after.starts_with("# hand written\n\n[tui]\n"), "{after}");
        assert!(after.contains("mouse    =   false   # kept"), "{after}");
        assert!(after.contains("scroll_lines = 7"), "{after}");
        // And the alias is gone for good: a reload reads the canonical
        // name and a second save leaves the file alone.
        let reloaded = ProductConfig::load(&paths).unwrap();
        reloaded.save(&paths).unwrap();
        assert_eq!(fs::read_to_string(paths.config_file()).unwrap(), after);
    }
}

/// A refused save is refused before the file is touched: the bytes on disk
/// are the bytes that were there.
#[test]
fn a_semantic_refusal_leaves_the_file_byte_identical() {
    let (_tmp, paths, before) = hand_written_home();
    let mut config = ProductConfig::load(&paths).unwrap();
    // A working set naming a template with no description is refused by
    // the one rule set, so `set_key` never reaches a save at all.
    config.templates.insert(
        "blank".to_owned(),
        AgentTemplate {
            description: String::new(),
            ..template("codex", "m")
        },
    );
    assert!(config.set_key("roster.working_set", "blank").is_err());
    // And a config that went invalid another way is refused at the save.
    config.root_agent = Some("absent".to_owned());
    assert!(matches!(
        config.save_publication_aware(&paths),
        Err(ConfigSaveError::BeforePublication(_))
    ));
    assert_eq!(fs::read_to_string(paths.config_file()).unwrap(), before);
}

#[test]
fn section_scope_is_exact_and_unknown_controls_fail_visibly() {
    assert!(ProductConfig::parse("[tui]\nenabled = true\n").is_err());
    assert!(ProductConfig::parse("[orchestration]\nsandbox = true\n").is_err());
}

#[test]
fn publication_faults_report_the_actual_pointer_commit() {
    for template_target in [true, false] {
        for stage in [
            AtomicStage::Write,
            AtomicStage::FileSync,
            AtomicStage::Publish,
            AtomicStage::ParentSync,
        ] {
            let (_tmp, paths) = PlakPaths::isolated_test_home();
            let mut old = ProductConfig::default_product();
            old.bind_root_tuple("grok", "old-model", None);
            old.save(&paths).unwrap();
            let mut candidate = ProductConfig::load(&paths).unwrap();
            candidate.bind_root_tuple("grok", "new-model", None);
            let mut seen = Vec::new();
            let error = candidate
                .save_with_fault(&paths, &mut |path, current| {
                    let is_template = path.parent() == Some(paths.user_agents_dir().as_path());
                    seen.push((is_template, current));
                    if is_template == template_target && current == stage {
                        Err("injected publication fault".into())
                    } else {
                        Ok(())
                    }
                })
                .unwrap_err();
            let committed = !template_target && stage == AtomicStage::ParentSync;
            assert_eq!(
                matches!(error, ConfigSaveError::PublishedDurabilityUncertain(_)),
                committed,
                "{template_target} {stage:?}: {error:?}"
            );
            let disk = ProductConfig::load(&paths).unwrap();
            assert_eq!(
                disk.default_model(),
                Some(if committed { "new-model" } else { "old-model" })
            );
            assert_eq!(
                fs::read_to_string(paths.user_agents_dir().join("default.toml")).unwrap(),
                render_template(old.root_template().unwrap())
            );
            if !template_target {
                let template_sync = seen
                    .iter()
                    .position(|row| *row == (true, AtomicStage::ParentSync))
                    .unwrap();
                let pointer_write = seen
                    .iter()
                    .position(|row| *row == (false, AtomicStage::Write))
                    .unwrap();
                assert!(template_sync < pointer_write);
            }
        }
    }
}

#[test]
fn second_template_failure_keeps_the_old_pointer() {
    let (_tmp, paths) = PlakPaths::isolated_test_home();
    let mut old = ProductConfig::default_product();
    old.bind_root_tuple("grok", "old-model", None);
    old.save(&paths).unwrap();
    let mut candidate = ProductConfig::load(&paths).unwrap();
    candidate
        .templates
        .insert("a-new".into(), template("grok", "new-a"));
    candidate
        .templates
        .insert("b-new".into(), template("grok", "new-b"));
    candidate.root_agent = Some("a-new".into());
    let error = candidate
        .save_with_fault(&paths, &mut |path, stage| {
            if path.file_name().is_some_and(|name| name == "b-new.toml")
                && stage == AtomicStage::Write
            {
                Err("second template refused".into())
            } else {
                Ok(())
            }
        })
        .unwrap_err();
    assert!(matches!(error, ConfigSaveError::BeforePublication(_)));
    assert!(paths.user_agents_dir().join("a-new.toml").is_file());
    assert!(!paths.user_agents_dir().join("b-new.toml").exists());
    assert_eq!(
        ProductConfig::load(&paths).unwrap().default_model(),
        Some("old-model")
    );
}

#[test]
fn template_preflight_refusal_creates_no_earlier_files() {
    let (_tmp, paths) = PlakPaths::isolated_test_home();
    let mut old = ProductConfig::default_product();
    old.templates
        .insert("z-existing".into(), template("grok", "old-model"));
    old.root_agent = Some("z-existing".into());
    old.save(&paths).unwrap();
    let mut candidate = ProductConfig::load(&paths).unwrap();
    candidate
        .templates
        .insert("a-new".into(), template("grok", "new-model"));
    candidate.templates.get_mut("z-existing").unwrap().model = "changed".into();
    assert!(matches!(
        candidate.save_publication_aware(&paths),
        Err(ConfigSaveError::BeforePublication(_))
    ));
    assert!(!paths.user_agents_dir().join("a-new.toml").exists());
    assert_eq!(
        ProductConfig::load(&paths).unwrap().default_model(),
        Some("old-model")
    );
}

#[test]
fn failed_pointer_publication_keeps_existing_root_binding() {
    let (_tmp, paths) = PlakPaths::isolated_test_home();
    let mut cfg = ProductConfig::default_product();
    cfg.bind_root_tuple("grok", "grok-4.6", None);
    cfg.save(&paths).unwrap();
    let before = ProductConfig::load(&paths).unwrap();
    let mut candidate = before.clone();
    candidate.bind_root_tuple("grok", "new-model", None);
    // Stop after prerequisite template publication, before the pointer
    // commit. This is exactly the config-write failure window.
    candidate.save_templates(&paths).unwrap();
    let reloaded = ProductConfig::load(&paths).unwrap();
    assert_eq!(reloaded.default_model(), before.default_model());
    assert_eq!(reloaded.root_agent, before.root_agent);
}

#[test]
fn parent_sync_failure_is_reported_after_config_publication() {
    let (_tmp, paths) = PlakPaths::isolated_test_home();
    let mut cfg = ProductConfig::default_product();
    cfg.bind_root_tuple("grok", "grok-4.6", None);
    cfg.save_templates(&paths).unwrap();

    let error = cfg
        .save_with_fault(&paths, &mut |target, stage| {
            if target == paths.config_file() && stage == AtomicStage::ParentSync {
                Err("injected directory sync failure".into())
            } else {
                Ok(())
            }
        })
        .unwrap_err();

    assert_eq!(
        error,
        ConfigSaveError::PublishedDurabilityUncertain("injected directory sync failure".into())
    );
    assert_eq!(
        ProductConfig::load(&paths).unwrap().default_account(),
        Some("grok")
    );
}

#[test]
fn save_refuses_an_invalid_config_before_publication() {
    let (_tmp, paths) = PlakPaths::isolated_test_home();
    let mut cfg = ProductConfig::default_product();
    cfg.templates
        .insert("broken".to_owned(), template("openai", "gpt-4o"));
    assert!(matches!(
        cfg.save_publication_aware(&paths),
        Err(ConfigSaveError::BeforePublication(_))
    ));
    assert!(!paths.config_file().is_file());
    assert!(!paths.user_agents_dir().join("broken.toml").exists());
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    #[test]
    fn config_render_parse_round_trip(
        account_index in 0_usize..4,
        model in "[a-z0-9._/-]{2,32}",
        effort in proptest::option::of("low|medium|high"),
        mouse in any::<bool>(),
        compact in any::<bool>(),
        theme_index in 0_usize..5,
    ) {
        let accounts = ["codex", "grok", "claude", "openrouter"];
        let themes = [
            ProductTheme::DefaultDark,
            ProductTheme::CatppuccinLatte,
            ProductTheme::CatppuccinFrappe,
            ProductTheme::CatppuccinMacchiato,
            ProductTheme::CatppuccinMocha,
        ];
        let mut cfg = ProductConfig::default_product();
        cfg.bind_root_tuple(accounts[account_index], &model, effort.as_deref());
        cfg.tui_mouse = mouse;
        cfg.compaction_enabled = compact;
        cfg.theme = themes[theme_index];
        let back = ProductConfig::parse(&cfg.render()).unwrap();
        // The main file carries everything but the template bodies.
        let mut expected = cfg.clone();
        expected.templates.clear();
        assert_eq!(back, expected);
        let template = cfg.root_template().unwrap();
        assert_eq!(parse_template(&render_template(template), "default").unwrap(), *template);
    }
}

#[test]
fn jobs_keys_are_closed_bounded_and_round_trip() {
    let defaults = ProductConfig::default_product();
    assert_eq!(defaults.jobs_foreground_seconds, 30);
    assert_eq!(defaults.jobs_max_runtime_hours, 10);
    let config = ProductConfig::parse("[jobs]\nforeground_seconds = 600\nmax_runtime_hours = 24\n")
        .expect("valid jobs");
    assert_eq!(
        config.job_config().expect("bounds").foreground_seconds(),
        600
    );
    assert_eq!(
        ProductConfig::parse(&config.render()).expect("round trip"),
        config
    );
    for (key, value) in [
        ("jobs.foreground_seconds", "0"),
        ("jobs.foreground_seconds", "601"),
        ("jobs.max_runtime_hours", "0"),
        ("jobs.max_runtime_hours", "25"),
    ] {
        let mut candidate = defaults.clone();
        assert!(candidate.set_key(key, value).is_err());
        assert_eq!(candidate, defaults);
    }
    assert!(ProductConfig::parse("[jobs]\nforeground_seconds = true\n").is_err());
    assert!(ProductConfig::parse("[jobs]\nunknown = 1\n").is_err());
}
