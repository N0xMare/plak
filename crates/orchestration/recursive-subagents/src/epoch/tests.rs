use super::*;
use crate::tree::text::PLAK_DELEGATION_EXPLICIT;

/// An epoch whose templates list no optional tools serializes and
/// digests exactly as it did before cards could carry tools: the fixture
/// hashes were taken from the encoder that predates the two fields.
#[test]
fn an_epoch_without_tools_keeps_its_pre_tool_encoding_and_digest() {
    const PRE_TOOL_JSON_SHA256: &str =
        "9c08a5f8a5cae045d5ad9e3654bdd16e9734ea8cf739f1d57877dcd6a6287110";
    const PRE_TOOL_DIGEST: &str =
        "af96b17cbebc7f36cdd4c5388742ef90b1400e6c8bbe32d53ade5a65eb94ebf3";
    let snapshot = OrchestrationSnapshot::freeze(RosterInput::test_working_set("fixture-tuple"));
    let json = serde_json::to_string(&snapshot).unwrap();
    assert_eq!(sha256_hex(&[json.as_bytes()]), PRE_TOOL_JSON_SHA256);
    assert_eq!(snapshot.digest, PRE_TOOL_DIGEST);
    assert!(!json.contains("\"tools\""));
    assert!(!json.contains("tool_sections"));
}

/// A card's runner is placement authority: set, it is inside the stored
/// snapshot and rolls the digest; unset, the card encodes as it always did.
#[test]
fn a_card_runner_is_inside_the_snapshot_and_digest_only_when_set() {
    let base = RosterInput::test_working_set("fixture-tuple");
    let plain = OrchestrationSnapshot::freeze(base.clone());
    assert!(
        !serde_json::to_string(&plain)
            .unwrap()
            .contains("\"runner\"")
    );

    let mut placed = base;
    let review = TemplateName::parse("review").unwrap();
    placed.templates.get_mut(&review).unwrap().runner = Some("build-box".to_owned());
    let placed = OrchestrationSnapshot::freeze(placed);
    assert_ne!(placed.digest, plain.digest);
    assert!(placed.digest_matches());
    let json = serde_json::to_string(&placed).unwrap();
    assert!(json.contains(r#""runner":"build-box""#));
    let back: OrchestrationSnapshot = serde_json::from_str(&json).unwrap();
    assert_eq!(back, placed);
    assert_eq!(
        back.binding(&review).unwrap().runner.as_deref(),
        Some("build-box")
    );
}

/// A card's tool list and the frozen tool tables are bind authority:
/// both roll the digest, both round-trip through the stored JSON.
#[test]
fn frozen_tools_and_tool_tables_are_inside_the_digest() {
    let base = RosterInput::test_working_set("fixture-tuple");
    let plain = OrchestrationSnapshot::freeze(base.clone());

    let mut with_card_tools = base.clone();
    let review = TemplateName::parse("review").unwrap();
    with_card_tools.templates.get_mut(&review).unwrap().tools =
        vec!["web_search".to_owned(), "mcp:github".to_owned()];
    let carded = OrchestrationSnapshot::freeze(with_card_tools);
    assert_ne!(carded.digest, plain.digest);
    assert!(carded.digest_matches());

    let mut with_tables = base;
    with_tables.tool_sections = BTreeMap::from([(
        "mcp_servers.github".to_owned(),
        "[mcp_servers.github]\ntransport = \"stdio\"\n".to_owned(),
    )]);
    let tabled = OrchestrationSnapshot::freeze(with_tables.clone());
    assert_ne!(tabled.digest, plain.digest);
    with_tables.tool_sections.insert(
        "mcp_servers.github".to_owned(),
        "[mcp_servers.github]\ntransport = \"http\"\n".to_owned(),
    );
    assert_ne!(
        OrchestrationSnapshot::freeze(with_tables).digest,
        tabled.digest
    );

    for snapshot in [carded, tabled] {
        let json = serde_json::to_string(&snapshot).unwrap();
        let reread: OrchestrationSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(reread, snapshot);
        assert!(reread.digest_matches());
    }
}

#[test]
fn a_card_names_its_template_tools_after_the_model() {
    let mut card = RosterBinding::leaf("codex", "gpt-6-luna");
    card.template = "scout".to_owned();
    card.show_model = true;
    assert_eq!(
        card.capability_line(),
        "tier ≤ mutate · spawns none · adopts no · model gpt-6-luna"
    );
    card.tools = vec!["x_search".to_owned(), "mcp:github".to_owned()];
    assert_eq!(
        card.capability_line(),
        "tier ≤ mutate · spawns none · adopts no · model gpt-6-luna · tools x_search, mcp:github"
    );
}

#[test]
fn unpinned_has_an_empty_menu_and_a_stable_digest() {
    let snapshot = OrchestrationSnapshot::unpinned("selected-tuple");
    assert!(snapshot.roster.is_empty());
    assert!(snapshot.digest_matches());
    let again = OrchestrationSnapshot::unpinned("selected-tuple");
    assert_eq!(snapshot.digest, again.digest);
    assert_eq!(snapshot.max_depth, DEFAULT_GRANT_DEPTH);
    assert_eq!(snapshot.max_children, DEFAULT_DIRECT_CHILDREN as u8);
    assert_eq!(snapshot.max_nodes, DEFAULT_TREE_NODES as u8);
    assert!(snapshot.template_cards(None).contains("(none;"));
    assert!(
        snapshot
            .template_cards(None)
            .starts_with(PLAK_DELEGATION_EXPLICIT)
    );
}

#[test]
fn show_model_puts_the_tuple_on_the_card() {
    let mut binding = RosterBinding::leaf("test-account", "grok-4.6");
    binding.spawns = vec![TemplateName::parse("worker").unwrap()];
    binding.may_adopt = true;
    assert_eq!(
        binding.capability_line(),
        "tier ≤ mutate · spawns worker · adopts yes"
    );
    binding.show_model = true;
    assert_eq!(
        binding.capability_line(),
        "tier ≤ mutate · spawns worker · adopts yes · model grok-4.6"
    );
}

#[test]
fn a_leaf_card_says_it_spawns_none_and_a_menu_omits_its_own_template() {
    let mut binding = RosterBinding::leaf("test-account", "m");
    binding.template = "lead".to_owned();
    assert_eq!(
        binding.capability_line(),
        "tier ≤ mutate · spawns none · adopts no"
    );
    binding.spawns = ["lead", "worker", "reviewer"]
        .map(|name| TemplateName::parse(name).unwrap())
        .to_vec();
    assert_eq!(
        binding.capability_line(),
        "tier ≤ mutate · spawns worker, reviewer · adopts no"
    );
}

/// The root spawns from the working set; every other node from its own
/// card, minus itself and minus anything the closure does not carry.
#[test]
fn each_node_menu_comes_from_its_own_card() {
    let name = |name: &str| TemplateName::parse(name).unwrap();
    let mut input = RosterInput::test_working_set("m");
    let mut nested = RosterBinding::leaf(RosterInput::TEST_ACCOUNT, "m");
    nested.template = "nested".to_owned();
    input.templates.insert(name("nested"), nested);
    let orchestrate = input.templates.get_mut(&name("orchestrate")).unwrap();
    orchestrate.spawns = vec![name("orchestrate"), name("nested"), name("absent")];
    let snapshot = OrchestrationSnapshot::freeze(input);
    let menu = |caller: Option<&str>| -> Vec<String> {
        snapshot
            .spawnable_template_names(caller)
            .map(ToString::to_string)
            .collect()
    };
    assert_eq!(
        menu(None),
        ["explore", "implement", "orchestrate", "review"]
    );
    assert_eq!(menu(Some("orchestrate")), ["nested"]);
    assert!(menu(Some("review")).is_empty(), "a leaf spawns nothing");
    assert_eq!(menu(Some("unknown")), [] as [String; 0]);
    assert!(snapshot.menu_contains(Some("orchestrate"), &name("nested")));
    assert!(!snapshot.menu_contains(None, &name("nested")));
    let cards = snapshot.template_cards(Some("orchestrate"));
    assert!(cards.contains("- nested: "), "{cards}");
    assert!(!cards.contains("- review: "), "{cards}");
}

/// The digest covers the closure: a template reachable only through a
/// `spawns` list, and the working set itself, both roll it.
#[test]
fn the_digest_covers_the_closure_and_the_working_set() {
    let name = |name: &str| TemplateName::parse(name).unwrap();
    let base = RosterInput::test_working_set("m");
    let mut nested_input = base.clone();
    let mut nested = RosterBinding::leaf(RosterInput::TEST_ACCOUNT, "m");
    nested.template = "nested".to_owned();
    nested_input.templates.insert(name("nested"), nested);
    let mut narrowed = base.clone();
    narrowed.working_set.remove(&name("review"));
    let base = OrchestrationSnapshot::freeze(base);
    let nested = OrchestrationSnapshot::freeze(nested_input);
    let narrowed = OrchestrationSnapshot::freeze(narrowed);
    assert_ne!(base.digest, nested.digest);
    assert_ne!(base.digest, narrowed.digest);
    let mut edited = nested.clone();
    edited
        .roster
        .get_mut(&name("nested"))
        .unwrap()
        .spawns
        .push(name("review"));
    assert!(!edited.digest_matches());
}
