//! Frozen template roster and tree-epoch snapshot.
//!
//!
//! The root spawns from the working set; every other node spawns from its own
//! card's `spawns` list. The snapshot freezes the closure of both, and it, not
//! the live config or the model's catalog, is bind authority for the rest of
//! the tree epoch; its digest covers the resolved template contents, so a
//! mid-epoch template edit is ignored until the successor epoch installs.

use std::collections::{BTreeMap, BTreeSet};

use plakous::model::sha256_hex;
use serde::{Deserialize, Serialize};

use crate::cell_budget::CEILING_MAX_AGENTS_PER_CELL;
use crate::protocol::{
    DEFAULT_GRANT_DEPTH, Delegation, MAX_DIRECT_CHILDREN, MAX_TREE_ATTEMPTS, MAX_TREE_DEPTH,
    MAX_TREE_NODES, TemplateName, Tier, WriterMode,
};
use crate::tree::text::{
    PLAK_TEMPLATE_CARDS_HEADER, PLAK_TEMPLATE_CARDS_NONE, delegation_policy_sentence,
};
use plakous::model::ProcessingTierDisposition;
use plakous_host_protocol::selection::{CompositionSelection, ProtocolSelection};

/// One template frozen into one tree epoch: its bind tuple plus the
/// authority the spawner reads.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RosterBinding {
    /// Template file name this binding was frozen from.
    pub template: String,
    /// The account this card binds; it must be in the epoch's
    /// [`BindHints`]. A child never takes its caller's account.
    pub account: String,
    /// The exact model this card binds.
    pub tuple_id: String,
    /// Reasoning effort the card asks for, when it asks for one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// How the child's tools are composed, when the card chooses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub composition: Option<CompositionSelection>,
    /// Which provider protocol the child speaks, when the card chooses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<ProtocolSelection>,
    /// Identity-bearing tier, derived from the template's `fast_mode`. A
    /// child runs on this tier whatever its caller runs on.
    pub processing_tier: ProcessingTierDisposition,
    /// The user's card text; the model routes on it.
    pub description: String,
    /// Tier ceiling: a spawn asking above this is clipped, never refused.
    pub max_tier: Tier,
    /// The optional tools the card enables, as the template lists them
    /// (`web_search`, `mcp:github`, `mcp:github/create_issue`). The product
    /// validated the list at load; the tree freezes it and spells none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<String>,
    /// The templates a node on this card may spawn, in the card's order.
    /// Empty is a leaf: such a node compiles no `spawn_agent`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub spawns: Vec<TemplateName>,
    /// Whether it may land a candidate.
    pub may_adopt: bool,
    /// Which delegation sentence a node on this card renders to its model.
    pub delegation: Delegation,
    /// Static, user-written label; never a measured cost.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_tier: Option<u8>,
    /// Turn ceiling for one attempt on this card.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_turns: Option<u32>,
    /// A command the host runs in a freshly minted copy before the child's
    /// first turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_mint: Option<String>,
    /// Environment for the `on_mint` hook only; not passed to the copy's tool calls.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Whether the card's capability line may name the model. Off by default,
    /// so a spawn menu never leaks the roster's bind.
    pub show_model: bool,
}

impl RosterBinding {
    /// A leaf card on `account` and `tuple_id`: Standard tier, no effort,
    /// leaf authority (`max_tier = mutate`, no spawn, no adopt). Unit tests
    /// pin these and mutate the fields they care about.
    #[must_use]
    pub fn leaf(account: impl Into<String>, tuple_id: impl Into<String>) -> Self {
        Self {
            template: String::new(),
            account: account.into(),
            tuple_id: tuple_id.into(),
            effort: None,
            composition: None,
            protocol: None,
            processing_tier: ProcessingTierDisposition::Standard,
            description: String::new(),
            max_tier: Tier::Mutate,
            tools: Vec::new(),
            spawns: Vec::new(),
            may_adopt: false,
            delegation: Delegation::Explicit,
            cost_tier: None,
            max_turns: None,
            on_mint: None,
            env: BTreeMap::new(),
            show_model: false,
        }
    }

    /// The spawn menu of a node on this card: its `spawns` list minus its
    /// own template.
    pub fn spawn_menu(&self) -> impl Iterator<Item = &TemplateName> + '_ {
        self.spawns
            .iter()
            .filter(move |name| name.as_str() != self.template)
    }

    /// Host-computed capability line under the user's description:
    /// tier ceiling, the card's own spawn menu, adopt, cost tier, model when
    /// shown.
    #[must_use]
    pub fn capability_line(&self) -> String {
        let menu: Vec<&str> = self.spawn_menu().map(TemplateName::as_str).collect();
        let spawns = if menu.is_empty() {
            "none".to_owned()
        } else {
            menu.join(", ")
        };
        let mut line = format!(
            "tier ≤ {} · spawns {spawns} · adopts {}",
            self.max_tier.as_str(),
            yes_no(self.may_adopt)
        );
        if let Some(cost_tier) = self.cost_tier {
            line.push_str(&format!(" · cost tier {cost_tier}"));
        }
        if self.show_model && !self.tuple_id.is_empty() {
            line.push_str(&format!(" · model {}", self.tuple_id));
        }
        line
    }
}

const fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

/// The product's account roster, frozen into the epoch.
///
/// The tree admits or refuses a template's pinned account by *membership*
/// alone: it never spells an account name, so which accounts exist and which
/// speak no native protocol are the product's
/// facts, supplied once at freeze time. Empty sets mean the epoch pins
/// nothing — every binding is refused — which is what the bootstrap
/// placeholder wants.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct BindHints {
    /// Accounts a template binding may pin at all.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pinnable: BTreeSet<String>,
    /// Accounts on which no model admits `native-ptc`, so such a card is
    /// refused at spawn. An account that admits it on some models only (the
    /// OpenAI API's CodeModeOnly rows) is not listed: the product decides
    /// that per model when it pins the child.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    no_native_protocol: BTreeSet<String>,
}

impl BindHints {
    /// The product's roster as two membership sets.
    #[must_use]
    pub fn new(pinnable: BTreeSet<String>, no_native_protocol: BTreeSet<String>) -> Self {
        Self {
            pinnable,
            no_native_protocol,
        }
    }

    /// No account is pinnable: the bootstrap placeholder, and any epoch whose
    /// `snapshot_json` predates the hints.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pinnable.is_empty() && self.no_native_protocol.is_empty()
    }

    /// May a template binding pin `account` at all?
    #[must_use]
    pub fn admits_account(&self, account: &str) -> bool {
        self.pinnable.contains(account)
    }

    /// Does `account` speak no native protocol?
    #[must_use]
    pub fn refuses_native_protocol(&self, account: &str) -> bool {
        self.no_native_protocol.contains(account)
    }

    /// Hints built from two name lists, for a caller that has the roster as
    /// slices — a test fixture, or a product whose roster is `&[&str]`.
    #[must_use]
    pub fn from_names(pinnable: &[&str], no_native_protocol: &[&str]) -> Self {
        Self::new(name_set(pinnable), name_set(no_native_protocol))
    }
}

fn name_set(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

/// Genesis freeze of root bind + working set. Child bind reads this, not a
/// later `config.toml` load.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct OrchestrationSnapshot {
    /// Frozen checkout reclamation policy.
    pub workspace_idle_seconds: u16,
    /// Soft disk budget over this tree's fork checkouts, in MiB.
    pub workspace_budget_mib: u16,
    /// Age bound for dormant child conversations, in days.
    pub child_retention_days: u16,
    /// Count bound for retained dormant child conversations.
    pub child_retention_count: u16,
    /// Content digest over the epoch's bind authority; `digest_matches`
    /// recomputes it, so a snapshot edited in the file is detectable.
    pub digest: String,
    /// `[root] agent` name. `None` when the root was bound by an explicit
    /// `--account` / `--model` instead of a configured template.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_template: Option<String>,
    /// The account the root is bound to. Children bind their own cards'
    /// accounts, never this.
    pub root_account: String,
    /// The exact model the root bound at genesis.
    pub root_tuple: String,
    /// The root's reasoning effort, when it named one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_effort: Option<String>,
    /// Whether one lease or many may write the launch checkout for this epoch.
    ///
    pub writer_mode: WriterMode,
    /// Frozen bounds for this epoch. Genesis may only lower the appendix
    /// ceilings (4 / 8 / 32); running nodes stays at the appendix value.
    /// Deepest descendant this epoch admits.
    pub max_depth: u8,
    /// Direct children one node may hold at once.
    pub max_children: u8,
    /// Nodes the whole tree may hold at once.
    pub max_nodes: u8,
    /// Attempts this epoch may open; lower-only against
    /// `MAX_TREE_ATTEMPTS`.
    #[serde(default = "default_max_attempts")]
    pub max_attempts: u8,
    /// Agents one Code Mode cell may admit in this epoch;
    /// lower-only against `CEILING_MAX_AGENTS_PER_CELL`. Frozen here, like the
    /// four tree bounds, so a config edit never moves a running tree's
    /// ceiling, and inside the digest like them too: the digest covers this
    /// epoch's bind authority, and a ceiling that decides whether a spawn is
    /// admitted is part of it. Still written only when it departs from the
    /// ceiling, so the serialized shape is unchanged.
    #[serde(
        default = "default_max_agents_per_cell",
        skip_serializing_if = "is_default_max_agents_per_cell"
    )]
    pub max_agents_per_cell: u16,
    /// `adopt_candidate` must cite review evidence about the candidate.
    /// Off by default.
    #[serde(default)]
    pub require_review_evidence: bool,
    /// Maximum transient continuation rounds per attempt.
    pub transient_resumes: u8,
    /// The root's spawn menu: `[roster] working_set`.
    pub working_set: BTreeSet<TemplateName>,
    /// One frozen binding per template in the epoch's closure: the working
    /// set plus every template reachable through `spawns` lists.
    pub roster: BTreeMap<TemplateName, RosterBinding>,
    /// Digest of the resolved template contents behind `roster` and the root
    /// template, so a template file edit rolls the epoch digest.
    #[serde(default)]
    pub templates_digest: String,
    /// The root template's `delegation`, frozen so the root renders a policy
    /// sentence even when its template is not itself in the working set.
    #[serde(default)]
    pub root_delegation: Delegation,
    /// The product's account roster for this epoch. Bind
    /// authority for every pinned template account; the tree only checks
    /// membership.
    #[serde(default, skip_serializing_if = "BindHints::is_empty")]
    pub bind_hints: BindHints,
    /// The canonical rendering of every `[tools.<name>]` and
    /// `[mcp_servers.<id>]` table the closure and the root template reach,
    /// keyed `tools.<name>` / `mcp_servers.<id>`. A child binds these, never
    /// the live file, so editing an MCP command mid-epoch changes nothing in
    /// a running tree. Inside the digest only when non-empty, so an epoch
    /// without optional tools keeps the encoding and digest it always had.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tool_sections: BTreeMap<String, String>,
}

const fn default_max_attempts() -> u8 {
    MAX_TREE_ATTEMPTS as u8
}

const fn default_max_agents_per_cell() -> u16 {
    CEILING_MAX_AGENTS_PER_CELL
}

#[allow(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde skip_serializing_if takes a reference"
)]
fn is_default_max_agents_per_cell(value: &u16) -> bool {
    *value == CEILING_MAX_AGENTS_PER_CELL
}

/// Root bind frozen into a snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RootBind {
    /// The configured template the root was bound from, when it was bound
    /// from one; an explicit `--account`/`--model` root has none.
    pub template: Option<String>,
    /// The account the session actually bound.
    pub account: String,
    /// The exact model it bound.
    pub tuple_id: String,
    /// Reasoning effort, when the bind named one.
    pub effort: Option<String>,
    /// The root's delegation policy, frozen so the root renders a sentence
    /// even when its template is not itself in the working set.
    pub delegation: Delegation,
}

impl RootBind {
    /// A root bound by tuple alone: no template, no account, no effort.
    #[must_use]
    pub fn tuple_only(tuple_id: impl Into<String>) -> Self {
        Self {
            template: None,
            account: String::new(),
            tuple_id: tuple_id.into(),
            effort: None,
            delegation: Delegation::default(),
        }
    }
}

/// The five configured `[orchestration]` bounds.
/// Each is lower-only against its hard ceiling and is clamped
/// at [`OrchestrationSnapshot::freeze`], never accepted above it. `u16` so a
/// config can *ask* for more and be told no by the clamp rather than by the
/// parser.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EpochBounds {
    /// Deepest descendant the epoch admits.
    pub max_depth: u16,
    /// Direct children one node may hold at once.
    pub max_children: u16,
    /// Nodes the whole tree may hold at once.
    pub max_nodes: u16,
    /// Attempts the epoch may open.
    pub max_attempts: u16,
    /// Agents one Code Mode cell may admit.
    pub max_agents_per_cell: u16,
}

impl EpochBounds {
    /// The appendix ceilings, with the default grant depth.
    pub const DEFAULT: Self = Self {
        max_depth: DEFAULT_GRANT_DEPTH as u16,
        max_children: MAX_DIRECT_CHILDREN as u16,
        max_nodes: MAX_TREE_NODES as u16,
        max_attempts: MAX_TREE_ATTEMPTS as u16,
        max_agents_per_cell: CEILING_MAX_AGENTS_PER_CELL,
    };
}

impl Default for EpochBounds {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Everything one tree epoch freezes: the root bind, the
/// occupancy mode, the bounds, the evidence rule, the working set as
/// already-resolved cards, and a digest over the template contents behind
/// them. The tree reads nothing else — not a `ProductConfig`, not a template
/// file — so the product builds one of these (`from_product_config`) and the
/// epoch is bind authority from then on.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RosterInput {
    /// Dormant-checkout idle delay, in seconds.
    pub workspace_idle_seconds: u16,
    /// Soft budget over fork checkouts, in MiB.
    pub workspace_budget_mib: u16,
    /// Age bound for dormant child conversations, in days.
    pub child_retention_days: u16,
    /// Count bound for retained dormant child conversations.
    pub child_retention_count: u16,
    /// What the session bound the root to.
    pub root: RootBind,
    /// Whether one lease or many may write the launch checkout.
    pub writer_mode: WriterMode,
    /// The five bounds as written; `freeze` clamps them.
    pub bounds: EpochBounds,
    /// `adopt_candidate` must cite review evidence.
    pub require_review_evidence: bool,
    /// Maximum transient continuation rounds per attempt.
    pub transient_resumes: u8,
    /// The root's spawn menu, `[roster] working_set`.
    pub working_set: BTreeSet<TemplateName>,
    /// One resolved card per template in the closure: the working set plus
    /// every template reachable through `spawns` lists.
    pub templates: BTreeMap<TemplateName, RosterBinding>,
    /// Digest over the resolved template contents behind `templates` and the
    /// root template, so a template file edit rolls the epoch digest.
    pub templates_digest: String,
    /// The product's account roster, so the tree spells no
    /// account name of its own.
    pub bind_hints: BindHints,
    /// The canonical rendering of every tool table the closure and the root
    /// template reach, keyed `tools.<name>` / `mcp_servers.<id>`; see
    /// [`OrchestrationSnapshot::tool_sections`].
    pub tool_sections: BTreeMap<String, String>,
}

impl RosterInput {
    /// An empty working set over `tuple_id`: the bootstrap placeholder and
    /// the base unit tests pin templates onto.
    #[must_use]
    pub fn unpinned(tuple_id: impl Into<String>) -> Self {
        Self {
            root: RootBind::tuple_only(tuple_id),
            workspace_idle_seconds: 600,
            workspace_budget_mib: 4096,
            child_retention_days: 7,
            child_retention_count: 128,
            writer_mode: WriterMode::SingleLease,
            bounds: EpochBounds::DEFAULT,
            require_review_evidence: false,
            transient_resumes: 1,
            working_set: BTreeSet::new(),
            templates: BTreeMap::new(),
            templates_digest: String::new(),
            bind_hints: BindHints::default(),
            tool_sections: BTreeMap::new(),
        }
    }

    /// The account the unit-test working set's cards bind.
    #[cfg(any(test, feature = "test-support"))]
    pub const TEST_ACCOUNT: &str = "test-account";

    /// The unit-test working set: `review` and `explore` are read-only
    /// leaves, `implement` is a mutating leaf, `orchestrate` is a read-only
    /// supervisor that may spawn the other three and adopt. Every card binds
    /// [`Self::TEST_ACCOUNT`] on `tuple_id` until a test pins another.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn test_working_set(tuple_id: impl Into<String>) -> Self {
        let tuple_id = tuple_id.into();
        let mut input = Self::unpinned(tuple_id.clone());
        input.bind_hints = BindHints::from_names(&[Self::TEST_ACCOUNT], &[]);
        let parse = |name: &str| {
            TemplateName::parse(name)
                .unwrap_or_else(|error| unreachable!("test template names are valid: {error}"))
        };
        let leaves = ["review", "explore", "implement"];
        for (name, max_tier) in [
            ("review", Tier::ReadOnly),
            ("explore", Tier::ReadOnly),
            ("implement", Tier::Mutate),
            ("orchestrate", Tier::ReadOnly),
        ] {
            let mut binding = RosterBinding::leaf(Self::TEST_ACCOUNT, tuple_id.clone());
            binding.template = name.to_owned();
            binding.max_tier = max_tier;
            if name == "orchestrate" {
                binding.spawns = leaves.iter().copied().map(parse).collect();
                binding.may_adopt = true;
            }
            input.working_set.insert(parse(name));
            input.templates.insert(parse(name), binding);
        }
        input
    }
}

impl OrchestrationSnapshot {
    /// An epoch with an empty working set: the bootstrap placeholder before
    /// `install_snapshot`, and the base unit tests pin templates onto.
    pub fn unpinned(tuple_id: impl Into<String>) -> Self {
        Self::freeze(RosterInput::unpinned(tuple_id))
    }

    /// Freezes one epoch. The bounds are clamped to their
    /// appendix ceilings here, and the digest is computed over the result.
    #[must_use]
    pub fn freeze(input: RosterInput) -> Self {
        let RosterInput {
            root,
            workspace_idle_seconds,
            workspace_budget_mib,
            child_retention_days,
            child_retention_count,
            writer_mode,
            bounds,
            require_review_evidence,
            transient_resumes,
            working_set,
            templates,
            templates_digest,
            bind_hints,
            tool_sections,
        } = input;
        let mut snapshot = Self {
            digest: String::new(),
            workspace_idle_seconds,
            workspace_budget_mib,
            child_retention_days,
            child_retention_count,
            root_delegation: root.delegation,
            root_template: root.template,
            root_account: root.account,
            root_tuple: root.tuple_id,
            root_effort: root.effort,
            writer_mode,
            max_depth: clamp_bound(bounds.max_depth, MAX_TREE_DEPTH),
            max_children: clamp_bound(bounds.max_children, MAX_DIRECT_CHILDREN as u8),
            max_nodes: clamp_bound(bounds.max_nodes, MAX_TREE_NODES as u8),
            max_attempts: clamp_bound(bounds.max_attempts, MAX_TREE_ATTEMPTS as u8),
            max_agents_per_cell: crate::cell_budget::clamp_max_agents_per_cell(
                bounds.max_agents_per_cell,
            ),
            require_review_evidence,
            transient_resumes: transient_resumes.min(2),
            working_set,
            roster: templates,
            templates_digest,
            bind_hints,
            tool_sections,
        };
        snapshot.digest = snapshot.compute_digest();
        snapshot
    }

    /// Recomputes the epoch digest from the snapshot's own bind authority.
    ///
    /// Every frozen bound, including `max_agents_per_cell`: an epoch's bind
    /// authority is one thing, and a ceiling that decides whether a spawn is
    /// admitted belongs in the digest with the other four.
    #[must_use]
    pub fn compute_digest(&self) -> String {
        let mut body = serde_json::json!({
            "root_template": self.root_template,
            "root_account": self.root_account,
            "root_tuple": self.root_tuple,
            "root_effort": self.root_effort,
            "writer_mode": self.writer_mode,
            "max_depth": self.max_depth,
            "max_children": self.max_children,
            "max_nodes": self.max_nodes,
            "workspace_idle_seconds": self.workspace_idle_seconds,
            "workspace_budget_mib": self.workspace_budget_mib,
            "child_retention_days": self.child_retention_days,
            "child_retention_count": self.child_retention_count,
            "max_attempts": self.max_attempts,
            "max_agents_per_cell": self.max_agents_per_cell,
            "require_review_evidence": self.require_review_evidence,
            "transient_resumes": self.transient_resumes,
            "working_set": self.working_set,
            "roster": self.roster,
            "templates_digest": self.templates_digest,
            "root_delegation": self.root_delegation,
            "bind_hints": self.bind_hints,
        });
        if !self.tool_sections.is_empty()
            && let Some(fields) = body.as_object_mut()
        {
            fields.insert(
                "tool_sections".to_owned(),
                serde_json::json!(self.tool_sections),
            );
        }
        let canonical = serde_json::to_vec(&body).unwrap_or_default();
        sha256_hex(&[&canonical])
    }

    /// Whether the stored digest still matches the contents, so a snapshot
    /// edited in the file is detected at reopen rather than trusted.
    #[must_use]
    pub fn digest_matches(&self) -> bool {
        self.digest == self.compute_digest()
    }

    /// The frozen card for one template, or `None` when this epoch's closure
    /// does not carry it — which is what makes a spawn `template_unknown`.
    #[must_use]
    pub fn binding(&self, template: &TemplateName) -> Option<&RosterBinding> {
        self.roster.get(template)
    }

    /// The spawn menu for a node bound to `caller_template`. The root (no
    /// template) spawns from the working set, in name order; any other node
    /// spawns from its own card's `spawns` list minus itself, in the card's
    /// order. A name the closure does not carry is never on a menu.
    pub fn spawnable_template_names<'a>(
        &'a self,
        caller_template: Option<&'a str>,
    ) -> Box<dyn Iterator<Item = &'a TemplateName> + 'a> {
        let Some(caller) = caller_template else {
            return Box::new(
                self.working_set
                    .iter()
                    .filter(|name| self.roster.contains_key(*name)),
            );
        };
        let card = self
            .roster
            .iter()
            .find(|(name, _)| name.as_str() == caller)
            .map(|(_, binding)| binding);
        Box::new(
            card.into_iter()
                .flat_map(RosterBinding::spawn_menu)
                .filter(move |name| name.as_str() != caller && self.roster.contains_key(*name)),
        )
    }

    /// Whether `template` is on the spawn menu of a node bound to
    /// `caller_template`.
    #[must_use]
    pub fn menu_contains(&self, caller_template: Option<&str>, template: &TemplateName) -> bool {
        self.spawnable_template_names(caller_template)
            .any(|name| name == template)
    }

    /// The caller's own `delegation`, from its frozen binding; a caller with
    /// no roster binding (the root, whose template need not be in the
    /// closure) falls back to the frozen root policy.
    #[must_use]
    pub fn delegation_for(&self, caller_template: Option<&str>) -> Delegation {
        caller_template
            .and_then(|name| {
                self.roster
                    .iter()
                    .find(|(key, _)| key.as_str() == name)
                    .map(|(_, binding)| binding.delegation)
            })
            .unwrap_or(self.root_delegation)
    }

    /// The spawn block: exactly one delegation sentence, then
    /// one card per spawnable template — the user's description plus the
    /// host-computed capability line. Never names accounts or tuples unless
    /// the template opted in with `show_model`.
    #[must_use]
    pub fn template_cards(&self, caller_template: Option<&str>) -> String {
        let mut lines = vec![
            delegation_policy_sentence(self.delegation_for(caller_template)).to_owned(),
            String::new(),
            PLAK_TEMPLATE_CARDS_HEADER.to_owned(),
        ];
        for name in self.spawnable_template_names(caller_template) {
            let Some(binding) = self.roster.get(name) else {
                continue;
            };
            lines.push(format!(
                "- {name}: {} ({})",
                binding.description,
                binding.capability_line()
            ));
        }
        if lines.len() == 3 {
            lines.push(PLAK_TEMPLATE_CARDS_NONE.to_owned());
        }
        lines.join("\n")
    }
}

/// A configured value above its appendix ceiling is clamped down here, never
/// accepted.
fn clamp_bound(configured: u16, ceiling: u8) -> u8 {
    u8::try_from(configured.min(u16::from(ceiling)))
        .unwrap_or(ceiling)
        .max(1)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests {
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
        let snapshot =
            OrchestrationSnapshot::freeze(RosterInput::test_working_set("fixture-tuple"));
        let json = serde_json::to_string(&snapshot).unwrap();
        assert_eq!(sha256_hex(&[json.as_bytes()]), PRE_TOOL_JSON_SHA256);
        assert_eq!(snapshot.digest, PRE_TOOL_DIGEST);
        assert!(!json.contains("\"tools\""));
        assert!(!json.contains("tool_sections"));
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
    fn unpinned_has_an_empty_menu_and_a_stable_digest() {
        let snapshot = OrchestrationSnapshot::unpinned("selected-tuple");
        assert!(snapshot.roster.is_empty());
        assert!(snapshot.digest_matches());
        let again = OrchestrationSnapshot::unpinned("selected-tuple");
        assert_eq!(snapshot.digest, again.digest);
        assert_eq!(snapshot.max_depth, DEFAULT_GRANT_DEPTH);
        assert_eq!(snapshot.max_children, MAX_DIRECT_CHILDREN as u8);
        assert_eq!(snapshot.max_nodes, MAX_TREE_NODES as u8);
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
}
