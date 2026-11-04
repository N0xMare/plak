//! Session policy, orchestration ceilings, and agent templates.

use super::load::{ProductConfig, is_template_account};
use super::{ProductCompositionSelection, ProductProtocolSelection, RootTemplateFact};
use crate::home::paths::PlakPaths;
use plakous::model::ProcessingTierDisposition;
use plakous_hosted::agent_tree_protocol::{
    Delegation, MAX_DIRECT_CHILDREN, MAX_TREE_ATTEMPTS, MAX_TREE_DEPTH, MAX_TREE_NODES,
    MAX_WORKING_SET, TemplateName, Tier,
};
use plakous_hosted::tool_plan::TemplateToolSelection;
use std::collections::BTreeMap;

/// The product's answer to "is a topology admitted now?", as a probe a hosted
/// session can hold without knowing where the answer comes from.
///
/// This is the product side of `runtime::HostConfig::topology_admission`: the
/// one place `[orchestration] subagents` is re-read mid-session, refusing unreadable
/// configuration exactly as [`SessionPolicy::load`] does. The runtime holds the closure, not
/// this file.
#[must_use]
pub fn topology_admission_probe(
    paths: &PlakPaths,
) -> std::sync::Arc<dyn Fn() -> plakous_hosted::CliResult<bool> + Send + Sync> {
    let paths = paths.clone();
    std::sync::Arc::new(move || {
        SessionPolicy::load(&paths)
            .map(|policy| policy.topology_enabled)
            .map_err(plakous_hosted::CliFailure::new)
    })
}

/// The hosted-session policy facts an attach reads out of the config file.
///
/// A session attach needs three answers — is scheduled compaction on, is the
/// topology enabled, and which template (if any) `[root] agent` names. Before
/// the slice each answer cost its own whole-file [`ProductConfig::load`]; the
/// slice is produced once by [`SessionPolicy::load`] and handed down.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionPolicy {
    /// `[compaction] enabled`: scheduled compaction for this session.
    pub compaction_enabled: bool,
    /// `[skills] enabled`: Agent Skills discovery at session construction.
    pub skills_enabled: bool,
    /// `[turn] max_model_rounds`: the round cap each root turn is admitted under.
    pub max_model_rounds: usize,
    /// Construction-bound process budgets, passed explicitly to hosting.
    pub jobs_foreground_seconds: u16,
    pub jobs_max_runtime_hours: u16,
    /// `[api] full_window`: construction-bound API window selection.
    pub api_full_window: bool,
    /// `[orchestration] subagents`: whether the session may open a topology.
    pub topology_enabled: bool,
    /// Wake idle interactive roots on unread child results or questions.
    pub auto_continue: bool,
    /// `[host] resume_after_loss`: hold (the default when unset) or continue.
    pub resume_after_loss: plakous_hosted::runtime::ResumeAfterLoss,
    /// `[orchestration] max_concurrent_requests_per_account`.
    pub requests_per_account: u16,
    /// `[root] agent` as a fact the runtime compares its bound tuple against.
    pub root_template: Option<RootTemplateFact>,
}

impl SessionPolicy {
    /// An absent file uses schema defaults. Unreadable configuration or
    /// templates refuse construction rather than freezing a disabled topology.
    pub fn load(paths: &PlakPaths) -> Result<Self, String> {
        ProductConfig::load(paths).map(|config| config.session_policy())
    }

    /// The schema defaults, for a home that has no config file yet.
    #[must_use]
    pub fn defaults() -> Self {
        ProductConfig::default_product().session_policy()
    }

    /// Sizes the process-wide per-account request budgets every session
    /// built after this call draws on, root and children alike.
    pub fn install_request_budget(&self) {
        if let Some(limit) = std::num::NonZeroUsize::new(usize::from(self.requests_per_account)) {
            plakous_hosted::request_slots::set_requests_per_account(limit);
        }
    }
}

/// `[telemetry]` retention as the file states it.
///
/// The store's own [`crate::telemetry_store::TelemetryRetentionConfig`] is
/// built from this by `From`, so no telemetry code reads `ProductConfig`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TelemetryConfig {
    pub max_bytes: u64,
    pub max_age_days: u64,
}

impl TelemetryConfig {
    /// The retention behind `paths`, or the schema defaults when the file
    /// cannot be read.
    #[must_use]
    pub fn load(paths: &PlakPaths) -> Self {
        ProductConfig::load_or_default(paths).telemetry()
    }
}

/// `[root]`: the default binding tuple new sessions start from.
///
/// The tuple, not the file: a caller resolving "what would a fresh session
/// bind" takes this instead of the whole [`ProductConfig`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DefaultBinding {
    pub account: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub extra: Option<String>,
    pub composition: ProductCompositionSelection,
    pub protocol: ProductProtocolSelection,
}

impl DefaultBinding {
    /// No `[root] agent`: every field is the product default.
    #[must_use]
    pub fn unbound() -> Self {
        Self {
            account: None,
            model: None,
            effort: None,
            extra: None,
            composition: ProductCompositionSelection::FollowCatalog,
            protocol: ProductProtocolSelection::Auto,
        }
    }

    /// The binding behind `paths`, or the unbound default when the file cannot
    /// be read.
    #[must_use]
    pub fn load(paths: &PlakPaths) -> Self {
        ProductConfig::load(paths)
            .map_or_else(|_| Self::unbound(), |config| config.default_binding())
    }

    /// The configured composition only for this exact tuple; see
    /// [`ProductConfig::composition_for_tuple`].
    #[must_use]
    pub fn composition_for_tuple(
        &self,
        account: Option<&str>,
        model: Option<&str>,
    ) -> ProductCompositionSelection {
        match (account, model) {
            (Some(account), Some(model))
                if self.account.as_deref() == Some(account)
                    && self.model.as_deref() == Some(model) =>
            {
                self.composition
            }
            _ => ProductCompositionSelection::FollowCatalog,
        }
    }

    /// Whether this binding asks for the priority processing tier on the exact
    /// `account`/`model` tuple.
    #[must_use]
    pub fn prefers_priority(&self, account: &str, model: &str) -> bool {
        self.account.as_deref() == Some(account)
            && self.model.as_deref() == Some(model)
            && self.extra.as_deref() == Some("priority")
    }
}

/// Hard orchestration ceilings. `[orchestration]` may only lower them.
pub const CEILING_MAX_DEPTH: u16 = MAX_TREE_DEPTH as u16;
pub const CEILING_MAX_CHILDREN: u16 = MAX_DIRECT_CHILDREN as u16;
pub const CEILING_MAX_NODES: u16 = MAX_TREE_NODES as u16;

/// `[orchestration] max_attempts` ceiling.
pub const CEILING_MAX_ATTEMPTS: u16 = MAX_TREE_ATTEMPTS as u16;

/// `[orchestration] max_concurrent_requests_per_account` ceiling.
pub const CEILING_REQUESTS_PER_ACCOUNT: u16 = 64;

/// `[turn] max_model_rounds` default and ceiling (spec 07 §3). The ceiling is
/// the runtime's own; the subscription gate is sized for it.
pub const DEFAULT_MAX_MODEL_ROUNDS: u16 = crate::product_agent::NATIVE_RUN_MAX_MODEL_ROUNDS as u16;
pub const CEILING_MAX_MODEL_ROUNDS: u16 =
    crate::product_agent::NATIVE_RUN_MAX_MODEL_ROUNDS_CEILING as u16;

/// `[tui] scroll_lines` default and bounds (spec 8.1).
pub const DEFAULT_SCROLL_LINES: u16 = 3;

/// Lower bound for `[tui] scroll_lines`.
pub const MIN_SCROLL_LINES: u16 = 1;

/// Upper bound for `[tui] scroll_lines`.
pub const MAX_SCROLL_LINES: u16 = 10;

/// Template minted by pre-attach shell binding when `[root] agent` is unset.
pub const MINTED_ROOT_TEMPLATE_NAME: &str = "default";

/// `[a-z][a-z0-9-]{0,31}`, and never a reserved section name.
pub fn validate_template_name(name: &str) -> Result<(), String> {
    TemplateName::parse(name).map(|_| ())
}

/// One complete, named Plakous construction plus its spawn authority.
/// Authored as one flat file,
/// `~/.plak/agents/<name>.toml` or `<project>/.plak/agents/<name>.toml`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentTemplate {
    pub account: String,
    pub model: String,
    pub effort: Option<String>,
    /// Closed to `priority` on `codex`.
    pub extra: Option<String>,
    pub composition: Option<ProductCompositionSelection>,
    pub protocol: Option<ProductProtocolSelection>,
    /// The card text a spawning model routes on.
    pub description: String,
    /// `access`: spawn tier ceiling; a spawn asking above it is clipped.
    pub max_tier: Tier,
    /// `tools`: the optional tools this template enables by name.
    pub tools: TemplateToolSelection,
    /// The templates a copy of this one may spawn. Empty is a leaf.
    pub spawns: Vec<TemplateName>,
    pub may_adopt: bool,
    pub delegation: Delegation,
    /// Static, user-written label; never a measured cost.
    pub cost_tier: Option<u8>,
    pub max_turns: Option<u32>,
    /// Hook run once when a copy is minted.
    pub on_mint: Option<String>,
    /// Environment for the `on_mint` hook only; not passed to the copy's tool calls.
    pub env: BTreeMap<String, String>,
    /// Put the model id on the card.
    pub show_model: bool,
}

impl Default for AgentTemplate {
    fn default() -> Self {
        Self {
            account: String::new(),
            model: String::new(),
            effort: None,
            extra: None,
            composition: None,
            protocol: None,
            description: String::new(),
            max_tier: Tier::Mutate,
            tools: TemplateToolSelection::default(),
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
}

impl AgentTemplate {
    /// Validate the template identity and authority. Its
    /// (`effort`) is a catalog question resolved at readiness and at bind,
    /// never guessed at parse. `description` is required only where a card is
    /// rendered, and `spawns` names only other files: [`ProductConfig::validate`]
    /// checks both.
    pub fn validate(&self, name: &str) -> Result<(), String> {
        if self.composition == Some(ProductCompositionSelection::PySdk) {
            return Err(format!(
                "template {name}: py-sdk is root-only and has no child result channel; select it in [root] or --composition"
            ));
        }
        if self.account == "xai" {
            return Err(
                "`xai` is ambiguous: use `grok` (SuperGrok subscription) or `xai-api` (API key)"
                    .to_owned(),
            );
        }
        if self.account.is_empty() {
            return Err(format!("template {name} requires a non-empty account"));
        }
        if self.model.is_empty() {
            return Err(format!("template {name} requires a non-empty model"));
        }
        if !is_template_account(&self.account) {
            return Err(format!(
                "template {name} account is not a supported template account"
            ));
        }
        self.validate_composition(self.composition)
            .map_err(|error| format!("template {name}: {error}"))?;
        if self.account != "openrouter"
            && self
                .protocol
                .is_some_and(|protocol| protocol != ProductProtocolSelection::Auto)
        {
            return Err(format!("template {name} protocol is OpenRouter-only"));
        }
        if let Some(provider) =
            plakous_hosted::binding::PlatformProvider::for_account(&self.account)
            && plakous_hosted::capabilities::for_account(&self.account)
                .is_some_and(|route| route.compiled)
        {
            plakous_hosted::platform::bundled_catalog(provider, "offline")
                .admit(
                    &self.model,
                    self.effort
                        .as_deref()
                        .and_then(crate::platform_catalog::api_effort),
                )
                .map_err(|error| {
                    format!(
                        "template {name}: {}",
                        crate::refusal_text::failure_text(
                            &plakous_hosted::CliFailure::from(error),
                            crate::notice_text::RemedyContext {
                                frontend: crate::notice_text::Frontend::Run,
                                session_id: None
                            }
                        )
                    )
                })?;
        }
        match self.extra.as_deref() {
            None => {}
            Some("priority") if self.account == "codex" => {}
            Some("priority") => {
                return Err(format!(
                    "template {name} Fast mode requires the Codex subscription account"
                ));
            }
            Some(_) => {
                return Err(format!(
                    "template {name} the provider option is unsupported"
                ));
            }
        }
        // A node's menu excludes itself, so a list of only this template is
        // still a leaf. Adoption needs a subtree that list can actually create.
        if self.may_adopt && !self.spawns.iter().any(|child| child.as_str() != name) {
            return Err(format!(
                "template {name} may_adopt requires spawns naming another template"
            ));
        }
        if self.spawns.len() > MAX_WORKING_SET {
            return Err(format!(
                "template {name} spawns names {} templates; the maximum is {MAX_WORKING_SET}",
                self.spawns.len()
            ));
        }
        for (index, child) in self.spawns.iter().enumerate() {
            if self.spawns[..index].contains(child) {
                return Err(format!(
                    "template {name} spawns names \"{child}\" more than once"
                ));
            }
        }
        if self.max_turns == Some(0) {
            return Err(format!("template {name} max_turns must be 1 or more"));
        }
        Ok(())
    }

    pub(super) fn validate_composition(
        &self,
        composition: Option<ProductCompositionSelection>,
    ) -> Result<(), String> {
        if self.account == "openrouter"
            && composition == Some(ProductCompositionSelection::NativePtc)
        {
            return Err(plakous_hosted::binding::OPENROUTER_NATIVE_PTC_REFUSAL.to_owned());
        }
        if let (Some(provider), Some(composition)) = (
            plakous_hosted::binding::PlatformProvider::for_account(&self.account),
            composition,
        ) {
            plakous_hosted::platform::api_composition(provider, &self.model, composition)?;
        }
        Ok(())
    }

    /// Reasoning tier, derived from `extra` and never from a catalog default.
    #[must_use]
    pub fn processing_tier(&self) -> ProcessingTierDisposition {
        match self.extra.as_deref() {
            Some("priority") => plakous::model::ProviderServiceTierId::new("priority")
                .map_or(ProcessingTierDisposition::Standard, |tier| {
                    ProcessingTierDisposition::ProviderNamed(tier)
                }),
            _ => ProcessingTierDisposition::Standard,
        }
    }

    #[must_use]
    pub fn is_fast(&self) -> bool {
        self.extra.as_deref() == Some("priority")
    }

    #[must_use]
    pub fn composition_or_default(&self) -> ProductCompositionSelection {
        self.composition
            .unwrap_or(ProductCompositionSelection::FollowCatalog)
    }

    #[must_use]
    pub fn protocol_or_default(&self) -> ProductProtocolSelection {
        self.protocol.unwrap_or(ProductProtocolSelection::Auto)
    }
}

/// Effective-configuration equality: every persisted field, none of the
/// provenance bookkeeping (`origins`, `template_origins`, `project`,
/// `warnings`, `present`). The match is written out so a new persisted field
/// cannot join the struct without deciding whether it is part of equality.
impl SessionPolicy {
    /// The window selection `[api] full_window` binds for a new API session.
    #[must_use]
    pub const fn api_window(&self) -> plakous_hosted::platform::ApiWindow {
        if self.api_full_window {
            plakous_hosted::platform::ApiWindow::Full
        } else {
            plakous_hosted::platform::ApiWindow::Working
        }
    }

    /// The skills setting `[skills] enabled` binds, with the user skills root
    /// under the login home (`~/.agents/skills`).
    #[must_use]
    pub fn skills(&self) -> plakous_hosted::skills::SkillsSetting {
        plakous_hosted::skills::SkillsSetting::new(
            self.skills_enabled,
            crate::home::paths::user_home(),
        )
    }
}

#[cfg(test)]
pub(super) fn template(account: &str, model: &str) -> AgentTemplate {
    AgentTemplate {
        account: account.to_owned(),
        model: model.to_owned(),
        description: format!("{model} worker"),
        ..AgentTemplate::default()
    }
}
