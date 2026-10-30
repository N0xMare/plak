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
    AgentTemplate, ProductConfig, no_native_protocol_accounts, template_accounts,
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
        template_accounts().map(str::to_owned).collect(),
        no_native_protocol_accounts().map(str::to_owned).collect(),
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
mod tests;
