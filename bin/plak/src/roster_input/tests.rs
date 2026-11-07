use super::*;
use crate::product_config::{ProductCompositionSelection, ProductProtocolSelection};
use plakous::model::ProcessingTierDisposition;
use plakous_host_protocol::selection::{CompositionSelection, ProtocolSelection};
use plakous_hosted::agent_tree::text::{
    PLAK_DELEGATION_EXPLICIT, PLAK_DELEGATION_PROACTIVE, PLAK_TEMPLATE_CARDS_HEADER,
};
use plakous_hosted::agent_tree_protocol::{
    Delegation, MAX_TREE_ATTEMPTS, MAX_TREE_DEPTH, MAX_TREE_NODES, Tier, WorkingSet,
};
use plakous_hosted::cell_budget::CEILING_MAX_AGENTS_PER_CELL;

fn template(account: &str, model: &str) -> AgentTemplate {
    AgentTemplate {
        account: account.to_owned(),
        model: model.to_owned(),
        description: format!("{model} worker"),
        ..AgentTemplate::default()
    }
}

fn name(value: &str) -> TemplateName {
    TemplateName::parse(value).unwrap()
}

#[test]
fn one_shared_source_resolves_each_saved_profile_without_selector_leakage() {
    use plakous_hosted::agent::RosterSource as _;
    use plakous_hosted::sessions::RootProfile;

    let source = ProductRosterSource::new(None);
    let first = RootProfile {
        root_template: Some("claude-default-root".into()),
        working_set: vec!["claude-default-worker".into()],
    };
    let second = RootProfile {
        root_template: Some("codex-default-root".into()),
        working_set: vec!["codex-default-reviewer".into()],
    };
    let home = source.roster("claude", "frozen-model", None, None).unwrap();
    for selected in [&first, &second, &first] {
        let roster = source
            .roster("claude/work", "frozen-model", Some("high"), Some(selected))
            .unwrap();
        assert_eq!(
            roster.root.template.as_deref(),
            selected.root_template.as_deref()
        );
        assert_eq!(roster.root.account, "claude/work");
        assert_eq!(roster.root.tuple_id, "frozen-model");
        assert_eq!(
            roster
                .working_set
                .iter()
                .map(|name| name.as_str())
                .collect::<Vec<_>>(),
            selected
                .working_set
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
        );
    }
    assert_eq!(
        source.roster("claude", "frozen-model", None, None).unwrap(),
        home
    );
    let removed = RootProfile {
        root_template: Some("removed-root".into()),
        working_set: first.working_set,
    };
    assert!(
        source
            .roster("claude", "frozen-model", None, Some(&removed))
            .is_err()
    );
}

#[test]
fn frozen_root_overrides_do_not_change_a_child_card_with_the_same_template_name() {
    use plakous_hosted::agent::RosterSource as _;
    let source = ProductRosterSource::new(None);
    let selection = plakous_hosted::sessions::RootProfile {
        root_template: Some("claude-default-root".into()),
        working_set: vec!["claude-default-root".into()],
    };
    let ordinary = source
        .roster("claude", "ordinary-root", None, Some(&selection))
        .unwrap();
    let frozen = source
        .roster("claude/work", "frozen-root", Some("high"), Some(&selection))
        .unwrap();
    assert_eq!(frozen.templates, ordinary.templates);
    assert_eq!(frozen.working_set, ordinary.working_set);
    assert_eq!(frozen.root.template.as_deref(), Some("claude-default-root"));
    assert_eq!(frozen.root.account, "claude/work");
    assert_eq!(frozen.root.tuple_id, "frozen-root");
}

#[test]
fn named_epoch_membership_is_explicit_and_preserves_vertical_protocol_rules() {
    let mut config = ProductConfig::default_product();
    let mut expected = Vec::new();
    for (index, vertical) in plakous_hosted::account::Vertical::ALL
        .into_iter()
        .enumerate()
    {
        let account = format!("{}/work", vertical.as_str());
        let template_name = format!("slot-{index}");
        config
            .templates
            .insert(template_name.clone(), template(&account, "fixture-model"));
        expected.push(account);
    }
    // Reach all seven cards without exceeding the working-set ceiling.
    let first = "slot-0".to_owned();
    config.working_set = WorkingSet::parse([first.as_str()]).unwrap();
    for index in 0..6 {
        config
            .templates
            .get_mut(&format!("slot-{index}"))
            .unwrap()
            .spawns
            .push(name(&format!("slot-{}", index + 1)));
    }
    let input = roster_input_from_config(&config, "claude/root", "fixture-model", None);
    assert!(!input.bind_hints.admits_account("claude/root"));
    assert!(!input.bind_hints.admits_account("claude/not-enrolled"));
    for account in expected {
        let selected: plakous_hosted::account::AccountRef = account.parse().unwrap();
        assert!(input.bind_hints.admits_account(&account), "{account}");
        assert_eq!(
            input.bind_hints.refuses_native_protocol(&account),
            no_native_protocol_accounts().any(|vertical| vertical == selected.vertical().as_str())
        );
        assert_eq!(
            input
                .templates
                .values()
                .find(|template| template.account == account)
                .unwrap()
                .account,
            account
        );
    }
    let default = roster_input_from_config(
        &ProductConfig::default_product(),
        "claude",
        "fixture-model",
        None,
    );
    assert_eq!(default.bind_hints, product_bind_hints());
    let named_root = roster_input_from_config(
        &ProductConfig::default_product(),
        "claude/work",
        "fixture-model",
        None,
    );
    assert_eq!(
        default.bind_hints, named_root.bind_hints,
        "a root-only account change must not enroll another child account"
    );
    assert_eq!(
        default.templates_digest, named_root.templates_digest,
        "a root-only account change must not alter the frozen template bytes"
    );
}

/// The epoch freezes each closure card's tool list and the tables those
/// lists (and the root template's) reach; an unreferenced table is not
/// epoch state, and spelling a default out changes nothing.
#[test]
fn the_epoch_freezes_reached_tool_tables_and_ignores_the_rest() {
    use plakous_hosted::tool_plan::TemplateToolSelection;
    let mut config = ProductConfig::parse(concat!(
        "[mcp_servers.github]\ntransport = \"stdio\"\ncommand = \"gh\"\n",
        "[mcp_servers.linear]\ntransport = \"stdio\"\ncommand = \"lin\"\n",
        "[mcp_servers.jira]\ntransport = \"stdio\"\ncommand = \"jira\"\n",
    ))
    .unwrap();
    let mut worker = template("codex", "gpt");
    worker.tools = TemplateToolSelection::parse(["web_search", "mcp:github"]).unwrap();
    config.templates.insert("worker".to_owned(), worker);
    let mut lead = template("codex", "gpt");
    lead.tools = TemplateToolSelection::parse(["mcp:linear"]).unwrap();
    config.templates.insert("lead".to_owned(), lead);
    config.root_agent = Some("lead".to_owned());
    config.working_set = WorkingSet::parse(vec!["worker".to_owned()]).unwrap();

    let snapshot = snapshot_from_config(&config, "codex", "gpt", None);
    assert_eq!(
        snapshot.roster[&name("worker")].tools,
        ["web_search", "mcp:github"]
    );
    assert_eq!(
        snapshot.tool_sections.keys().collect::<Vec<_>>(),
        [
            "mcp_servers.github",
            "mcp_servers.linear",
            "tools.web_search"
        ]
    );
    assert_eq!(
        snapshot.tool_sections["tools.web_search"],
        "[tools.web_search]\nmax_uses = 5\nallowed_domains = []\nblocked_domains = []\n"
    );
    assert!(snapshot.digest_matches());

    // An unreferenced table is not epoch state.
    let mut unreferenced = config.clone();
    unreferenced
        .tool_sections
        .mcp_servers
        .get_mut("jira")
        .unwrap()
        .call_seconds = 7;
    let same = snapshot_from_config(&unreferenced, "codex", "gpt", None);
    assert_eq!(same.digest, snapshot.digest);
    assert_eq!(same.templates_digest, snapshot.templates_digest);

    // Spelling the defaults out is the same epoch.
    let mut explicit = config.clone();
    explicit.tool_sections.web_search = Some(crate::product_config::WebSearchOptions::default());
    assert_eq!(
        snapshot_from_config(&explicit, "codex", "gpt", None).digest,
        snapshot.digest
    );

    // A referenced table (the closure's or the root's) rolls both digests.
    for server in ["github", "linear"] {
        let mut edited = config.clone();
        edited
            .tool_sections
            .mcp_servers
            .get_mut(server)
            .unwrap()
            .call_seconds = 7;
        let rolled = snapshot_from_config(&edited, "codex", "gpt", None);
        assert_ne!(rolled.digest, snapshot.digest, "{server}");
        assert_ne!(
            rolled.templates_digest, snapshot.templates_digest,
            "{server}"
        );
    }

    // An explicit --account/--model root binds no template, so the root
    // template's tables are not reached.
    let explicit_root = snapshot_from_config(&config, "claude", "opus", None);
    assert!(
        !explicit_root
            .tool_sections
            .contains_key("mcp_servers.linear")
    );
}

/// `max_agents_per_cell` is stored as written and clamped
/// where the other bounds are, so one rule covers all five. It is epoch
/// state on the snapshot — not a process static — and outside the digest.
#[test]
fn the_cell_bound_clamps_at_finalize_like_the_tree_bounds() {
    let mut config = ProductConfig::default_product();
    config.max_agents_per_cell = 4096;
    let input = roster_input_from_config(&config, "codex", "gpt", None);
    assert_eq!(input.bounds.max_agents_per_cell, 4096, "stored as written");
    let ceiling = OrchestrationSnapshot::freeze(input);
    assert_eq!(ceiling.max_agents_per_cell, CEILING_MAX_AGENTS_PER_CELL);
    config.max_agents_per_cell = 2;
    let two = snapshot_from_config(&config, "codex", "gpt", None);
    assert_eq!(two.max_agents_per_cell, 2);
    config.max_agents_per_cell = 0;
    let one = snapshot_from_config(&config, "codex", "gpt", None);
    assert_eq!(one.max_agents_per_cell, 1);
    // Inside the digest, like the other four bounds: a ceiling that
    // decides whether a spawn is admitted is bind authority, so two
    // epochs that differ only here are different epochs.
    assert_ne!(ceiling.digest, two.digest);
    assert_ne!(two.digest, one.digest);
    // A snapshot at the ceiling serializes exactly as it did before the
    // field existed; a lowered one carries it and round-trips.
    let at_ceiling = serde_json::to_string(&ceiling).unwrap();
    assert!(!at_ceiling.contains("max_agents_per_cell"));
    let lowered = serde_json::to_string(&two).unwrap();
    assert!(lowered.contains("\"max_agents_per_cell\":2"));
    let reread: OrchestrationSnapshot = serde_json::from_str(&lowered).unwrap();
    assert_eq!(reread.max_agents_per_cell, 2);
    let reread: OrchestrationSnapshot = serde_json::from_str(&at_ceiling).unwrap();
    assert_eq!(reread.max_agents_per_cell, CEILING_MAX_AGENTS_PER_CELL);
}

/// The `RosterInput` -> snapshot digest golden: the tree side of the
/// epoch is a pure function of the input, so a frozen digest pins both
/// the freeze and the digest body. Change either deliberately.
#[test]
fn roster_input_freezes_to_a_pinned_digest() {
    let mut input = RosterInput::unpinned("selected-tuple");
    input.root.template = Some("hub".to_owned());
    input.root.account = "codex".to_owned();
    input.root.effort = Some("high".to_owned());
    input.root.delegation = Delegation::Proactive;
    input.bounds = EpochBounds {
        max_depth: 2,
        max_children: 3,
        max_nodes: 32,
        max_attempts: 5,
        max_agents_per_cell: 4,
    };
    input.require_review_evidence = true;
    input.templates_digest = "0123abcd".to_owned();
    let mut worker = RosterBinding::leaf("codex", "gpt-5.6-luna");
    worker.template = "worker".to_owned();
    worker.description = "does the work".to_owned();
    worker.cost_tier = Some(2);
    worker.max_turns = Some(12);
    worker.on_mint = Some("cargo fetch".to_owned());
    worker
        .env
        .insert("CARGO_HOME".to_owned(), "/tmp/cargo".to_owned());
    input.templates.insert(name("worker"), worker);
    let mut critic = RosterBinding::leaf("openrouter", "vendor/model");
    critic.template = "critic".to_owned();
    critic.composition = Some(CompositionSelection::Direct);
    critic.protocol = Some(ProtocolSelection::Chat);
    critic.description = "reviews".to_owned();
    critic.max_tier = Tier::ReadOnly;
    critic.show_model = true;
    input.templates.insert(name("critic"), critic);
    input
        .templates
        .get_mut(&name("worker"))
        .unwrap()
        .spawns
        .push(name("critic"));
    input.working_set.insert(name("worker"));

    let snapshot = OrchestrationSnapshot::freeze(input.clone());
    assert_eq!(snapshot.max_depth, 2);
    assert_eq!(snapshot.max_children, 3);
    assert_eq!(snapshot.max_nodes, 32);
    assert_eq!(snapshot.max_attempts, 5);
    assert_eq!(snapshot.max_agents_per_cell, 4);
    assert!(snapshot.digest_matches());
    assert_eq!(
        snapshot.digest, "b9c843628834376f71fe17ecef99fe672591dbfe8c75a5903fb3ee5c98e767aa",
        "the RosterInput -> snapshot digest moved; update the pin deliberately"
    );
    assert_eq!(OrchestrationSnapshot::freeze(input).digest, snapshot.digest);
}

#[test]
fn configured_bounds_only_lower_and_clamp_at_finalize() {
    let mut config = ProductConfig::default_product();
    config.max_depth = 900;
    config.max_children = 3;
    config.max_nodes = 900;
    let snapshot = snapshot_from_config(&config, "codex", "gpt", None);
    assert_eq!(snapshot.max_depth, MAX_TREE_DEPTH);
    assert_eq!(snapshot.max_children, 3);
    assert_eq!(snapshot.max_nodes, MAX_TREE_NODES as u8);

    config.max_depth = 2;
    let lowered = snapshot_from_config(&config, "codex", "gpt", None);
    assert_eq!(lowered.max_depth, 2);
    assert_ne!(lowered.digest, snapshot.digest);

    // The attempt ceiling and the evidence rule are epoch
    // state too, lower-only and inside the digest.
    assert_eq!(snapshot.max_attempts, 64);
    assert!(!snapshot.require_review_evidence);
    config.max_attempts = 900;
    config.require_review_evidence = true;
    let ruled = snapshot_from_config(&config, "codex", "gpt", None);
    assert_eq!(usize::from(ruled.max_attempts), MAX_TREE_ATTEMPTS);
    assert!(ruled.require_review_evidence);
    assert_ne!(ruled.digest, lowered.digest);
    config.max_attempts = 5;
    assert_eq!(
        snapshot_from_config(&config, "codex", "gpt", None).max_attempts,
        5
    );
}

#[test]
fn transient_resume_budget_is_frozen_clamped_and_in_the_epoch_digest() {
    let mut config = ProductConfig::default_product();
    let original = snapshot_from_config(&config, "codex", "gpt", None);
    assert_eq!(original.transient_resumes, 1);
    config.transient_resumes = 2;
    let updated = snapshot_from_config(&config, "codex", "gpt", None);
    assert_eq!(updated.transient_resumes, 2);
    assert_ne!(updated.digest, original.digest);
    let mut edited = updated.clone();
    edited.transient_resumes = 0;
    assert!(!edited.digest_matches());
    config.transient_resumes = 256;
    assert_eq!(
        snapshot_from_config(&config, "codex", "gpt", None).transient_resumes,
        2
    );
}

#[test]
fn api_and_mixed_rosters_freeze_exact_accounts_and_inheritance() {
    let mut config = ProductConfig::default_product();
    for (name, account, model) in [
        ("openai-worker", "openai", "gpt-5.6-sol"),
        ("anthropic-worker", "anthropic", "claude-opus-5"),
        ("xai-worker", "xai-api", "grok-4.6"),
        ("oauth-worker", "codex", "gpt-5.6-luna"),
    ] {
        let mut worker = template(account, model);
        worker.effort = Some("medium".into());
        worker.composition = Some(ProductCompositionSelection::PyPtc);
        worker.validate(name).unwrap();
        config.templates.insert(name.into(), worker);
    }
    config
        .templates
        .insert("inherited".into(), AgentTemplate::default());
    config.working_set = WorkingSet::parse([
        "openai-worker",
        "anthropic-worker",
        "xai-worker",
        "oauth-worker",
        "inherited",
    ])
    .unwrap();
    let snapshot =
        snapshot_from_config(&config, "anthropic", "claude-sonnet-5", Some("low".into()));
    for (name, account) in [
        ("openai-worker", "openai"),
        ("anthropic-worker", "anthropic"),
        ("xai-worker", "xai-api"),
    ] {
        let binding = snapshot.binding(&self::name(name)).unwrap();
        assert_eq!(binding.account, account);
        assert_eq!(binding.effort.as_deref(), Some("medium"));
        assert_eq!(binding.composition, Some(CompositionSelection::PyPtc));
        assert!(snapshot.bind_hints.admits_account(account));
        assert_eq!(
            snapshot.bind_hints.refuses_native_protocol(account),
            account != "openai",
            "{account}"
        );
    }
    assert_eq!(
        snapshot.binding(&name("oauth-worker")).unwrap().account,
        "codex"
    );
    assert!(!snapshot.bind_hints.refuses_native_protocol("codex"));
    let inherited = snapshot.binding(&name("inherited")).unwrap();
    assert_eq!(inherited.account, "");
    assert_eq!(inherited.tuple_id, "");
    let restored: OrchestrationSnapshot =
        serde_json::from_str(&serde_json::to_string(&snapshot).unwrap()).unwrap();
    assert_eq!(restored, snapshot);
    assert!(restored.digest_matches());
    let mut changed = restored;
    changed.roster.get_mut(&name("xai-worker")).unwrap().account = "grok".into();
    assert!(!changed.digest_matches());
}

#[test]
fn openrouter_roster_protocol_freezes_on_snapshot_digest() {
    let mut config = ProductConfig::default_product();
    config.templates.insert(
        "router".to_owned(),
        AgentTemplate {
            account: "openrouter".into(),
            model: "vendor/model".into(),
            composition: Some(ProductCompositionSelection::Direct),
            protocol: Some(ProductProtocolSelection::Chat),
            description: "routed critic".into(),
            ..AgentTemplate::default()
        },
    );
    config.working_set = WorkingSet::parse(["router"]).unwrap();
    let snapshot = snapshot_from_config(&config, "codex", "gpt-5.6-luna", None);
    let pin = snapshot.binding(&name("router")).unwrap();
    assert_eq!(pin.template, "router");
    assert_eq!(pin.account, "openrouter");
    assert_eq!(pin.tuple_id, "vendor/model");
    assert_eq!(pin.protocol, Some(ProtocolSelection::Chat));
    assert!(snapshot.digest_matches());
    let mut mutated = snapshot.clone();
    mutated.roster.get_mut(&name("router")).unwrap().protocol = Some(ProtocolSelection::Messages);
    assert!(!mutated.digest_matches());
}

#[test]
fn template_authority_and_tier_are_inside_the_digest() {
    let mut config = ProductConfig::default_product();
    config.templates.insert("fast".to_owned(), {
        let mut template = template("codex", "gpt-5.6-luna");
        template.extra = Some("priority".to_owned());
        template
    });
    config
        .templates
        .insert("standard".to_owned(), template("codex", "gpt-5.6-luna"));
    config.working_set = WorkingSet::parse(["fast"]).unwrap();
    let fast = snapshot_from_config(&config, "codex", "gpt-5.6-luna", None);
    assert_eq!(
        fast.binding(&name("fast")).unwrap().processing_tier,
        ProcessingTierDisposition::ProviderNamed(
            plakous::model::ProviderServiceTierId::new("priority").unwrap()
        )
    );

    config.working_set = WorkingSet::parse(["standard"]).unwrap();
    let standard = snapshot_from_config(&config, "codex", "gpt-5.6-luna", None);
    assert_eq!(
        standard.binding(&name("standard")).unwrap().processing_tier,
        ProcessingTierDisposition::Standard
    );
    assert_ne!(fast.digest, standard.digest);

    // Same name, same tuple: an authority edit alone moves the digest.
    config.templates.get_mut("standard").unwrap().max_tier = Tier::ReadOnly;
    let narrowed = snapshot_from_config(&config, "codex", "gpt-5.6-luna", None);
    assert_eq!(
        narrowed.binding(&name("standard")).unwrap().max_tier,
        Tier::ReadOnly
    );
    assert_ne!(narrowed.digest, standard.digest);
}

/// the epoch digest covers template contents, not only the
/// main config, so a template edit that changes no `[roster]` line still
/// rolls the epoch.
#[test]
fn epoch_digest_covers_template_contents() {
    let mut config = ProductConfig::default_product();
    config
        .templates
        .insert("scribe".to_owned(), template("claude", "claude-opus-5"));
    config.working_set = WorkingSet::parse(["scribe"]).unwrap();
    let before = snapshot_from_config(&config, "codex", "gpt-5.6-luna", None);
    config.templates.get_mut("scribe").unwrap().on_mint = Some("cargo fetch".into());
    let after = snapshot_from_config(&config, "codex", "gpt-5.6-luna", None);
    assert_ne!(before.templates_digest, after.templates_digest);
    assert_ne!(before.digest, after.digest);
    // The root template counts even when it is outside the working set.
    config
        .templates
        .insert("primary".to_owned(), template("codex", "gpt-5.6-luna"));
    config.root_agent = Some("primary".to_owned());
    let with_root = snapshot_from_config(&config, "codex", "gpt-5.6-luna", None);
    config.templates.get_mut("primary").unwrap().effort = Some("high".into());
    let root_edited = snapshot_from_config(&config, "codex", "gpt-5.6-luna", None);
    assert_ne!(with_root.templates_digest, root_edited.templates_digest);
}

#[test]
fn root_template_name_enters_the_snapshot_only_for_the_bound_tuple() {
    let mut config = ProductConfig::default_product();
    config
        .templates
        .insert("primary".to_owned(), template("grok", "grok-4.6"));
    config.root_agent = Some("primary".to_owned());
    let bound = snapshot_from_config(&config, "grok", "grok-4.6", None);
    assert_eq!(bound.root_template.as_deref(), Some("primary"));

    let explicit = snapshot_from_config(&config, "codex", "gpt-5.6-luna", None);
    assert_eq!(explicit.root_template, None);
    assert_ne!(bound.digest, explicit.digest);
}

#[test]
fn working_set_excludes_the_caller_from_its_menu_and_cards() {
    let mut config = ProductConfig::default_product();
    config
        .templates
        .insert("hub".to_owned(), template("codex", "gpt-5.6-luna"));
    config.templates.get_mut("hub").unwrap().spawns = vec![name("critic"), name("hub")];
    config
        .templates
        .insert("critic".to_owned(), template("claude", "claude-opus-5"));
    config.templates.get_mut("critic").unwrap().max_tier = Tier::ReadOnly;
    config.templates.get_mut("critic").unwrap().cost_tier = Some(2);
    config.working_set = WorkingSet::parse(["hub", "critic"]).unwrap();
    let snapshot = snapshot_from_config(&config, "codex", "gpt-5.6-luna", None);
    assert_eq!(
        snapshot
            .spawnable_template_names(Some("hub"))
            .map(TemplateName::as_str)
            .collect::<Vec<_>>(),
        ["critic"]
    );
    assert_eq!(snapshot.spawnable_template_names(None).count(), 2);
    let cards = snapshot.template_cards(Some("hub"));
    assert!(cards.starts_with(PLAK_DELEGATION_EXPLICIT));
    assert!(cards.contains(&format!("\n\n{PLAK_TEMPLATE_CARDS_HEADER}\n")));
    assert!(cards.contains(
        "- critic: claude-opus-5 worker (tier ≤ read_only · spawns none · adopts no · cost tier 2)"
    ));
    assert!(!cards.contains("- hub:"));
    assert!(
        !cards.contains("claude-opus-5 ·"),
        "no model unless show_model"
    );
    assert!(!cards.contains("account:"));
    assert!(!cards.contains("claude:"), "no account name on the card");
}

/// A working-set name under another working-set card is one reference
/// line. The card's own name is not, and the reference does not walk
/// that sibling's children again.
#[test]
fn a_working_set_sibling_is_one_reference_line() {
    let roots = [name("zeta"), name("alpha")];
    let listed = nested_spawns(&roots, |parent| match parent.as_str() {
        "zeta" => vec![name("alpha"), name("zeta")],
        "alpha" => vec![name("note")],
        _ => Vec::new(),
    });
    let shape: Vec<_> = listed
        .iter()
        .map(|(depth, template)| (template.as_str(), *depth))
        .collect();
    assert_eq!(
        shape,
        [("zeta", 0), ("alpha", 1), ("alpha", 0), ("note", 1)]
    );
}

/// The epoch freezes the closure: the working set plus everything
/// reachable through `spawns` lists, cycles included. The root's menu is
/// the working set; each node's comes from its own card; the digest
/// covers the nested templates' contents.
#[test]
fn the_spawn_closure_is_frozen_and_each_menu_comes_from_its_own_card() {
    let mut config = ProductConfig::default_product();
    let mut lead = template("codex", "gpt-5.6-luna");
    lead.spawns = vec![name("worker"), name("critic")];
    lead.may_adopt = true;
    config.templates.insert("lead".to_owned(), lead);
    let mut worker = template("codex", "gpt-5.6-luna");
    worker.spawns = vec![name("lead")];
    config.templates.insert("worker".to_owned(), worker);
    config
        .templates
        .insert("critic".to_owned(), template("claude", "claude-opus-5"));
    config
        .templates
        .insert("unused".to_owned(), template("claude", "claude-opus-5"));
    config.working_set = WorkingSet::parse(["lead"]).unwrap();
    config.validate().unwrap();
    assert_eq!(
        spawn_closure(&config),
        [name("lead"), name("worker"), name("critic")]
    );
    let snapshot = snapshot_from_config(&config, "codex", "gpt-5.6-luna", None);
    let menu = |caller: Option<&str>| -> Vec<String> {
        snapshot
            .spawnable_template_names(caller)
            .map(ToString::to_string)
            .collect()
    };
    assert_eq!(
        snapshot
            .roster
            .keys()
            .map(TemplateName::as_str)
            .collect::<Vec<_>>(),
        ["critic", "lead", "worker"],
        "unused is outside the closure"
    );
    assert_eq!(menu(None), ["lead"]);
    assert_eq!(menu(Some("lead")), ["worker", "critic"]);
    assert_eq!(menu(Some("worker")), ["lead"], "a cycle is fine");
    assert_eq!(menu(Some("critic")), [] as [String; 0]);
    assert!(
        snapshot
            .template_cards(Some("lead"))
            .contains("- worker: gpt-5.6-luna worker (tier ≤ mutate · spawns lead · adopts no)")
    );
    config.templates.get_mut("critic").unwrap().effort = Some("high".into());
    let edited = snapshot_from_config(&config, "codex", "gpt-5.6-luna", None);
    assert_ne!(snapshot.templates_digest, edited.templates_digest);
    assert_ne!(snapshot.digest, edited.digest);
    config.templates.get_mut("unused").unwrap().effort = Some("high".into());
    let outside = snapshot_from_config(&config, "codex", "gpt-5.6-luna", None);
    assert_eq!(
        edited.digest, outside.digest,
        "unused is outside the digest"
    );
}

/// the spawn block opens with exactly one delegation
/// sentence, chosen by the caller's own template, and the root falls back
/// to the frozen root-template policy when it is outside the working set.
#[test]
fn each_delegation_value_renders_its_sentence_exactly_once_in_the_cards() {
    let mut config = ProductConfig::default_product();
    let mut hub = template("codex", "gpt-5.6-luna");
    hub.spawns = vec![name("critic")];
    hub.delegation = Delegation::Proactive;
    config.templates.insert("hub".to_owned(), hub);
    config
        .templates
        .insert("critic".to_owned(), template("claude", "claude-opus-5"));
    let mut root = template("codex", "gpt-5.6-sol");
    root.delegation = Delegation::Explicit;
    config.templates.insert("boss".to_owned(), root);
    config.root_agent = Some("boss".to_owned());
    config.working_set = WorkingSet::parse(["hub", "critic"]).unwrap();
    let snapshot = snapshot_from_config(&config, "codex", "gpt-5.6-sol", None);

    assert_eq!(snapshot.root_delegation, Delegation::Explicit);
    assert_eq!(snapshot.delegation_for(None), Delegation::Explicit);
    assert_eq!(snapshot.delegation_for(Some("hub")), Delegation::Proactive);

    let root_cards = snapshot.template_cards(None);
    assert!(root_cards.starts_with(PLAK_DELEGATION_EXPLICIT));
    assert_eq!(root_cards.matches(PLAK_DELEGATION_EXPLICIT).count(), 1);
    assert!(!root_cards.contains(PLAK_DELEGATION_PROACTIVE));

    let hub_cards = snapshot.template_cards(Some("hub"));
    assert!(hub_cards.starts_with(PLAK_DELEGATION_PROACTIVE));
    assert_eq!(hub_cards.matches(PLAK_DELEGATION_PROACTIVE).count(), 1);
    assert!(!hub_cards.contains(PLAK_DELEGATION_EXPLICIT));
}
