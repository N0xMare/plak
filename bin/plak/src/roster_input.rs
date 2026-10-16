//! The product's half of one tree epoch: `ProductConfig` -> `RosterInput`.
//!
//!
//! `plakous-recursive-subagents` freezes a `RosterInput` and reads nothing
//! else — not a `ProductConfig`, not a template file, not a rendered card. It
//! is the product that knows what a template file is, how it renders, and
//! which of them the working set names, so resolving one into the other is
//! the product's job and lives here.

use std::collections::{BTreeMap, BTreeSet};

use plakous::model::sha256_hex;

use crate::home::paths::PlakPaths;
use crate::product_config::{
    AgentTemplate, NO_NATIVE_PROTOCOL_ACCOUNTS, ProductConfig, TEMPLATE_ACCOUNTS,
};
use plakous_hosted::agent_tree_protocol::{Delegation, TemplateName};
use plakous_hosted::orchestration::{
    BindHints, EpochBounds, OrchestrationSnapshot, RootBind, RosterBinding, RosterInput,
};
use plakous_hosted::{CliFailure, CliResult};

/// Freezes one template file's contents into the epoch's card.
#[must_use]
pub fn roster_binding_from_template(
    template_name: &str,
    template: &AgentTemplate,
) -> RosterBinding {
    RosterBinding {
        template: template_name.to_owned(),
        account: template.account.clone(),
        tuple_id: template.model.clone(),
        effort: template.effort.clone(),
        // The epoch freezes the neutral selection vocabulary; the
        // product's own enums are its parsing and CLI face.
        composition: template.composition.map(Into::into),
        protocol: template.protocol.map(Into::into),
        processing_tier: template.processing_tier(),
        description: template.description.clone(),
        max_tier: template.max_tier,
        tools: template
            .tools
            .entries()
            .iter()
            .map(ToString::to_string)
            .collect(),
        spawns: template.spawns.clone(),
        may_adopt: template.may_adopt,
        delegation: template.delegation,
        cost_tier: template.cost_tier,
        max_turns: template.max_turns,
        on_mint: template.on_mint.clone(),
        env: template.env.clone(),
        show_model: template.show_model,
    }
}

/// The product-side convenience: [`roster_input_from_config`] then
/// `OrchestrationSnapshot::freeze`.
pub fn snapshot_from_config(
    config: &ProductConfig,
    root_account: impl Into<String>,
    root_tuple: impl Into<String>,
    root_effort: Option<String>,
) -> OrchestrationSnapshot {
    OrchestrationSnapshot::freeze(roster_input_from_config(
        config,
        root_account,
        root_tuple,
        root_effort,
    ))
}

/// Resolves a loaded `ProductConfig` into the epoch the tree freezes:
/// the cards of the working set and of every template reachable from it
/// through `spawns` lists, the root bind, the `[orchestration]` bounds as
/// written, and the digest over the rendered templates.
#[must_use]
pub fn roster_input_from_config(
    config: &ProductConfig,
    root_account: impl Into<String>,
    root_tuple: impl Into<String>,
    root_effort: Option<String>,
) -> RosterInput {
    let root_account = root_account.into();
    let root_tuple = root_tuple.into();
    let mut templates = BTreeMap::new();
    for name in spawn_closure(config) {
        let Some(template) = config.templates.get(name.as_str()) else {
            continue;
        };
        templates.insert(
            name.clone(),
            roster_binding_from_template(name.as_str(), template),
        );
    }
    // The root template names this bind only when the configured template
    // is the tuple actually bound; an explicit --account/--model root has
    // no template name.
    let root_template = config.root_agent.clone().filter(|_| {
        config.root_template().is_some_and(|template| {
            template.account == root_account && template.model == root_tuple
        })
    });
    let tool_sections = reached_tool_sections(
        config,
        templates.keys().map(TemplateName::as_str),
        root_template.as_deref(),
    );
    let templates_digest = templates_digest(
        config,
        templates.keys().map(TemplateName::as_str),
        root_template.as_deref(),
        &tool_sections,
    );
    RosterInput {
        workspace_idle_seconds: config.workspace_idle_seconds,
        workspace_budget_mib: config.workspace_budget_mib,
        child_retention_days: config.child_retention_days,
        child_retention_count: config.child_retention_count,
        root: RootBind {
            delegation: config
                .root_template()
                .map_or_else(Delegation::default, |template| template.delegation),
            template: root_template,
            account: root_account,
            tuple_id: root_tuple,
            effort: root_effort,
        },
        writer_mode: config.occupancy,
        bounds: EpochBounds {
            max_depth: config.max_depth,
            max_children: config.max_children,
            max_nodes: config.max_nodes,
            max_attempts: config.max_attempts,
            max_agents_per_cell: config.max_agents_per_cell,
        },
        require_review_evidence: config.require_review_evidence,
        transient_resumes: config.transient_resumes.min(2) as u8,
        working_set: config.working_set.names().iter().cloned().collect(),
        templates,
        templates_digest,
        bind_hints: product_bind_hints(),
        tool_sections,
    }
}

/// The canonical `[tools.*]` / `[mcp_servers.*]` tables the closure and the
/// root template reach. An unreferenced table is not frozen, so editing it
/// leaves the epoch alone.
fn reached_tool_sections<'a>(
    config: &ProductConfig,
    closure: impl Iterator<Item = &'a str>,
    root_template: Option<&'a str>,
) -> BTreeMap<String, String> {
    let mut reached = BTreeMap::new();
    for name in closure.chain(root_template) {
        if let Some(template) = config.templates.get(name) {
            reached.extend(config.tool_sections.reached(&template.tools));
        }
    }
    reached
}

/// The product's account roster as the two membership sets the tree checks.
/// Frozen into the epoch so the tree spells no account name.
#[must_use]
pub fn product_bind_hints() -> BindHints {
    BindHints::new(
        name_set(TEMPLATE_ACCOUNTS),
        name_set(NO_NATIVE_PROTOCOL_ACCOUNTS),
    )
}

/// The working set plus every template reachable from it through `spawns`
/// lists, each once, in discovery order. Cycles end where a name repeats;
/// the depth bounds, not this walk, stop a recursive tree. A working-set
/// name shown again as a profile-box reference is not a second member.
#[must_use]
pub fn spawn_closure(config: &ProductConfig) -> Vec<TemplateName> {
    let mut seen = BTreeSet::new();
    nested_spawns(config.working_set.names(), |name| {
        config
            .templates
            .get(name.as_str())
            .map(|template| template.spawns.clone())
            .unwrap_or_default()
    })
    .into_iter()
    .filter_map(|(_, name)| seen.insert(name.clone()).then_some(name))
    .collect()
}

/// Depth-first walk from `roots` through each template's `spawns`, each name
/// once, with its depth below the roots. The Agents box indents on it.
///
/// A working-set member named by another card is one reference line at that
/// depth and is not walked again. A card naming itself adds nothing: its menu
/// excludes its own template.
pub(crate) fn nested_spawns(
    roots: &[TemplateName],
    spawns: impl Fn(&TemplateName) -> Vec<TemplateName>,
) -> Vec<(usize, TemplateName)> {
    fn visit(
        name: &TemplateName,
        depth: usize,
        parent_is_self: bool,
        spawns: &dyn Fn(&TemplateName) -> Vec<TemplateName>,
        seen: &mut BTreeSet<TemplateName>,
        roots: &BTreeSet<TemplateName>,
        out: &mut Vec<(usize, TemplateName)>,
    ) {
        if !seen.insert(name.clone()) {
            if depth > 0 && !parent_is_self && roots.contains(name) {
                out.push((depth, name.clone()));
            }
            return;
        }
        out.push((depth, name.clone()));
        for child in spawns(name) {
            visit(
                &child,
                depth + 1,
                child.as_str() == name.as_str(),
                spawns,
                seen,
                roots,
                out,
            );
        }
    }
    let roots_set: BTreeSet<TemplateName> = roots.iter().cloned().collect();
    let mut seen = roots_set.clone();
    let mut out = Vec::new();
    for root in roots {
        out.push((0, root.clone()));
        for child in spawns(root) {
            visit(
                &child,
                1,
                child.as_str() == root.as_str(),
                &spawns,
                &mut seen,
                &roots_set,
                &mut out,
            );
        }
    }
    out
}

fn name_set(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

/// Digest over the resolved template contents this epoch depends on: every
/// template in the spawn closure plus the root template, in name order.
/// Canonical rendering, so a byte-identical semantic template produces the
/// same digest.
fn templates_digest<'a>(
    config: &ProductConfig,
    closure: impl Iterator<Item = &'a str>,
    root_template: Option<&'a str>,
    tool_sections: &BTreeMap<String, String>,
) -> String {
    let mut names: Vec<&str> = closure.collect();
    if let Some(root) = root_template
        && !names.contains(&root)
    {
        names.push(root);
    }
    names.sort_unstable();
    let mut body = String::new();
    for name in names {
        if let Some(template) = config.templates.get(name) {
            body.push_str(name);
            body.push('\n');
            body.push_str(&crate::product_config::render_template(template));
            body.push('\n');
        }
    }
    // The tool tables those templates reach, so editing a referenced
    // `[mcp_servers.*]` entry rolls the digest a successor epoch compares.
    for (key, rendered) in tool_sections {
        body.push_str(key);
        body.push('\n');
        body.push_str(rendered);
        body.push('\n');
    }
    sha256_hex(&[body.as_bytes()])
}

/// C8: that snapshot is authority for the whole epoch, so a file
/// the host cannot read or parse must fail closed rather than silently freeze
/// `default_product()` — an empty roster and default policy — as if that were
/// what the operator configured. An *absent* file is not a failure:
/// `ProductConfig::load` returns the schema defaults for it, so every `Err`
/// here is a genuine read or parse error.
/// The `config.toml` a tree genesis freezes into its `OrchestrationSnapshot`.
///
/// Fails closed: a file the host cannot read or parse must not silently freeze
/// `default_product()` — an empty roster and default policy — as if that were
/// what the operator configured. An *absent* file is not a failure.
pub fn genesis_orchestration_config(paths: Option<&PlakPaths>) -> CliResult<ProductConfig> {
    let Some(paths) = paths else {
        return Ok(ProductConfig::default_product());
    };
    ProductConfig::load(paths).map_err(|error| {
        CliFailure::new(format!(
            "config.toml could not be loaded: {error}; refusing to freeze a tree epoch from defaults"
        ))
    })
}

/// Plak's roster and topology gate: `config.toml` under one product home.
///
/// The product side of `agent::RosterSource`. `None` is a session with no
/// product home: the schema defaults are the honest answer for a roster
/// nothing configured, and the topology gate is closed.
pub(crate) struct ProductRosterSource {
    paths: Option<PlakPaths>,
}

impl ProductRosterSource {
    pub(crate) const fn new(paths: Option<PlakPaths>) -> Self {
        Self { paths }
    }
}

impl plakous_hosted::agent::RosterSource for ProductRosterSource {
    fn roster(
        &self,
        root_account: &str,
        root_tuple: &str,
        root_effort: Option<&str>,
    ) -> CliResult<RosterInput> {
        let config = genesis_orchestration_config(self.paths.as_ref())?;
        Ok(roster_input_from_config(
            &config,
            root_account,
            root_tuple,
            root_effort.map(str::to_owned),
        ))
    }

    fn account_name(&self, provider: plakous::subscriptions::NativeProvider) -> &'static str {
        crate::oauth_ui::account_cli_name(provider)
    }

    fn topology_admitted(&self) -> CliResult<bool> {
        self.paths.as_ref().map_or(Ok(false), |paths| {
            crate::product_config::SessionPolicy::load(paths)
                .map(|policy| policy.topology_enabled)
                .map_err(CliFailure::new)
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests {
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
        explicit.tool_sections.web_search =
            Some(crate::product_config::WebSearchOptions::default());
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
            max_nodes: 900,
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
        assert_eq!(snapshot.max_nodes, MAX_TREE_NODES as u8);
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
        assert_eq!(usize::from(snapshot.max_attempts), MAX_TREE_ATTEMPTS);
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
        mutated.roster.get_mut(&name("router")).unwrap().protocol =
            Some(ProtocolSelection::Messages);
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
            snapshot.template_cards(Some("lead")).contains(
                "- worker: gpt-5.6-luna worker (tier ≤ mutate · spawns lead · adopts no)"
            )
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
}
