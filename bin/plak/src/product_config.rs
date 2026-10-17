//! Product `config.toml` (non-secret preferences).

use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use toml_edit::Value;

use crate::home::paths::PlakPaths;

mod toml_file;
mod tool_sections;

pub use tool_sections::{
    DEFAULT_FETCH_URL_MAX_BYTES, DEFAULT_FETCH_URL_TIMEOUT_SECONDS, DEFAULT_MCP_CALL_SECONDS,
    DEFAULT_MCP_STARTUP_SECONDS, DEFAULT_WEB_SEARCH_MAX_USES, FetchUrlOptions, McpServerConfig,
    McpTransport, ToolSections, WebSearchOptions, XSearchOptions,
};

/// Per-repository trust for `.plak/config.toml` and `.plak/agents/`.
pub mod project_trust;

use plakous::model::ProcessingTierDisposition;
use plakous_hosted::agent_tree_protocol::{
    DEFAULT_DIRECT_CHILDREN, DEFAULT_TREE_ATTEMPTS, DEFAULT_TREE_NODES, Delegation,
    MAX_DIRECT_CHILDREN, MAX_TREE_ATTEMPTS, MAX_TREE_DEPTH, MAX_TREE_NODES, MAX_WORKING_SET,
    TemplateName, Tier, WorkingSet, WriterMode,
};
use plakous_hosted::tool_plan::{SelectedTool, TemplateToolSelection};
use std::collections::{BTreeMap, BTreeSet};

// Session-binding vocabulary lives in `plakous_hosted::binding`; these names stay for
// the readers that spell them the configuration way.
pub use plakous_hosted::binding::RootTemplateFact;

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

pub use plakous_hosted::binding::{
    CompositionSelection as ProductCompositionSelection,
    OpenRouterRoutingSelection as ProductOpenRouterRoutingSelection,
    ProtocolSelection as ProductProtocolSelection, ResolvedProtocol as ProductResolvedProtocol,
    TargetSelection as ProductTargetSelection,
    WIRE_RESOLUTION_REVISION as PRODUCT_WIRE_RESOLUTION_REVISION,
    WireBinding as ProductWireBinding,
};

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

/// The commented `[tools.*]` / `[mcp_servers.*]` example the annotated
/// config ends with. Every line is a comment: a table enables nothing, so the
/// reference pins none.
const TOOL_SECTIONS_REFERENCE: &str = "
# Optional-tool options. A table enables nothing: a template enables a tool by
# listing it in tools = [...]. A trusted project may add or replace entries by
# name. Secrets are never values here; name one stored with `plak secret set`.
#
# [tools.fetch_url]
# max_bytes = 2097152                 # 1..=8388608, body cap before conversion
# timeout_seconds = 30                # 1..=120, whole fetch with redirects
#
# [tools.web_search]
# max_uses = 5                        # 1..=20 searches per call
# allowed_domains = []                # at most 10; exclusive with blocked_domains
# blocked_domains = []                # at most 10
#
# [tools.x_search]
# allowed_x_handles = []              # at most 20 handles, without the @
#
# [mcp_servers.github]
# transport = \"stdio\"
# command = \"github-mcp-server\"       # resolved on PATH at spawn
# args = [\"stdio\"]
# env = { GITHUB_TOOLSETS = \"repos,issues\" }
# secret_env = { GITHUB_PERSONAL_ACCESS_TOKEN = \"github\" }
# tools = [\"get_file_contents\", \"create_issue\"]   # omit for every listed tool
# startup_seconds = 20                # 1..=120
# call_seconds = 120                  # 1..=3600 per call
#
# [mcp_servers.linear]
# transport = \"http\"
# url = \"https://mcp.linear.app/mcp\"  # https, or http to 127.0.0.1 or [::1]
# bearer_secret = \"linear\"            # Authorization: Bearer <secret>
# headers = { X-Client = \"plak\" }
# secret_headers = { X-Workspace = \"linear-ws\" }
";

#[cfg(test)]
mod tool_sections_tests;

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
        if self.account == "openrouter" {
            if self.composition == Some(ProductCompositionSelection::NativePtc) {
                return Err(format!(
                    "template {name}: {}",
                    plakous_hosted::binding::OPENROUTER_NATIVE_PTC_REFUSAL
                ));
            }
        } else if self
            .protocol
            .is_some_and(|protocol| protocol != ProductProtocolSelection::Auto)
        {
            return Err(format!("template {name} protocol is OpenRouter-only"));
        }
        if let Some(provider) =
            plakous_hosted::binding::PlatformProvider::for_account(&self.account)
        {
            plakous_hosted::platform::bundled_catalog(provider, "offline")
                .admit(
                    &self.model,
                    self.effort
                        .as_deref()
                        .and_then(crate::platform_catalog::api_effort),
                )
                .map_err(|error| format!("template {name}: {error}"))?;
            if let Some(composition) = self.composition {
                plakous_hosted::platform::api_composition(provider, &self.model, composition)
                    .map_err(|error| format!("template {name}: {error}"))?;
            }
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

/// Credential half of readiness. The catalog projection carries
/// only a `ready` bit; expiry is an auth-slot fact a caller passes directly.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CredentialReadiness {
    Ready,
    NotLoggedIn,
    Expired,
    NoApiKey,
}

/// Closed, operator-grade readiness disposition for one template. Never embeds
/// a token, a key prefix, or a raw provider error body.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TemplateReadiness {
    Ready,
    NotReady(TemplateNotReady),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TemplateNotReady {
    NotLoggedIn,
    CredentialExpired,
    NoApiKey,
    ModelNotAdmitted,
    EffortNotAdmitted,
    TierNotAdvertised,
}

impl TemplateNotReady {
    #[must_use]
    pub const fn reason(self) -> &'static str {
        match self {
            Self::NotLoggedIn => "not logged in",
            Self::CredentialExpired => "credential expired",
            Self::NoApiKey => "no API key",
            Self::ModelNotAdmitted => "model not admitted",
            Self::EffortNotAdmitted => "effort not admitted",
            Self::TierNotAdvertised => "tier not advertised",
        }
    }
}

impl TemplateReadiness {
    #[must_use]
    pub const fn is_ready(self) -> bool {
        matches!(self, Self::Ready)
    }

    #[must_use]
    pub const fn reason(self) -> Option<&'static str> {
        match self {
            Self::Ready => None,
            Self::NotReady(reason) => Some(reason.reason()),
        }
    }
}

/// One template projected for the Agents / Models tabs. Typed view only; the
/// TUI computes none of this and never reads `config.toml`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TemplateReadinessRow {
    pub name: String,
    pub template: AgentTemplate,
    pub readiness: TemplateReadiness,
}

/// Pure over one credential disposition plus the exact Ready catalog. Never
/// called from a draw path and never cached across a credential change.
#[must_use]
pub fn template_readiness(
    template: &AgentTemplate,
    credential: CredentialReadiness,
    catalog: &[plakous_host_protocol::views::ProviderSection],
) -> TemplateReadiness {
    match credential {
        CredentialReadiness::NotLoggedIn => {
            return TemplateReadiness::NotReady(TemplateNotReady::NotLoggedIn);
        }
        CredentialReadiness::Expired => {
            return TemplateReadiness::NotReady(TemplateNotReady::CredentialExpired);
        }
        CredentialReadiness::NoApiKey => {
            return TemplateReadiness::NotReady(TemplateNotReady::NoApiKey);
        }
        CredentialReadiness::Ready => {}
    }
    let Some(entry) = catalog
        .iter()
        .find(|section| section.ready && section.account == template.account)
        .and_then(|section| {
            section
                .entries
                .iter()
                .find(|entry| entry.id == template.model)
        })
    else {
        return TemplateReadiness::NotReady(TemplateNotReady::ModelNotAdmitted);
    };
    if entry.unavailable_reason.is_some() {
        return TemplateReadiness::NotReady(TemplateNotReady::ModelNotAdmitted);
    }
    let effort_admitted = match template.effort.as_deref() {
        None => true,
        Some(effort) => entry.effort_options.iter().any(|option| option == effort),
    };
    if !effort_admitted {
        return TemplateReadiness::NotReady(TemplateNotReady::EffortNotAdmitted);
    }
    let tier_admitted = match template.extra.as_deref() {
        None => true,
        Some(extra) => entry.extras.iter().any(|candidate| candidate.id == extra),
    };
    if !tier_admitted {
        return TemplateReadiness::NotReady(TemplateNotReady::TierNotAdvertised);
    }
    TemplateReadiness::Ready
}

/// Catalog-derived credential disposition. `Expired` is an auth-slot fact the
/// catalog projection cannot carry; a caller that knows it passes it directly.
#[must_use]
pub fn catalog_credential_readiness(
    account: &str,
    catalog: &[plakous_host_protocol::views::ProviderSection],
) -> CredentialReadiness {
    let ready = catalog
        .iter()
        .any(|section| section.ready && section.account == account);
    if ready {
        CredentialReadiness::Ready
    } else if account == "openrouter" {
        CredentialReadiness::NoApiKey
    } else {
        CredentialReadiness::NotLoggedIn
    }
}

/// Where an effective value was decided: the schema default, the user's
/// global `config.toml`, or a trusted project's `.plak/config.toml`.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ConfigValueOrigin {
    /// The key was not written anywhere; the schema default is in force.
    Default,
    /// The user's global `config.toml` carries the key.
    User,
    /// A trusted project's `.plak/config.toml` carries the key.
    Project,
}

impl ConfigValueOrigin {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::User => "user",
            Self::Project => "project",
        }
    }
}

/// The project `.plak` home a load discovered, and whether its configuration
/// was admitted. An untrusted project leaves its source named here while its
/// config and templates stay inactive.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectConfigSource {
    /// Canonical project root (the directory holding `.plak`).
    pub root: PathBuf,
    /// `true` once the operator has trusted this root; `false` means the
    /// project's config and templates were discovered but not applied.
    pub trusted: bool,
}

/// Where an explicit `config set`/settings save publishes: the global home or
/// the trusted project's `.plak` override.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ConfigWriteScope {
    /// `~/.plak/config.toml` and `~/.plak/agents/` only.
    #[default]
    User,
    /// `<project>/.plak/config.toml` and `<project>/.plak/agents/` only.
    Project,
}

impl ConfigWriteScope {
    /// The two spellings `--scope` accepts.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "user" | "global" => Some(Self::User),
            "project" => Some(Self::Project),
            _ => None,
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Project => "project",
        }
    }
}

/// Operator preferences (never secrets).
///
/// Equality compares the *effective* configuration only: provenance fields
/// (`origins`, `template_origins`, `project`, `warnings`, `present`) describe
/// where values came from, so a config built in memory and the same config
/// reloaded from disk are equal.
#[derive(Clone, Debug)]
pub struct ProductConfig {
    /// Templates resolved from `~/.plak/agents/*.toml` and, when the project
    /// is trusted, `.plak/agents/*.toml` (project wins on a name collision),
    /// keyed by name.
    pub templates: BTreeMap<String, AgentTemplate>,
    /// Which file each template came from, same keys as `templates`.
    pub template_origins: BTreeMap<String, ConfigValueOrigin>,
    /// Which file each documented key came from. Keys absent from the map
    /// mean the schema default is in force.
    pub origins: BTreeMap<ConfigField, ConfigValueOrigin>,
    /// The discovered project source, if any.
    pub project: Option<ProjectConfigSource>,
    /// Non-fatal load notices (for now: an untrusted project source naming
    /// what it would have applied).
    pub warnings: Vec<String>,
    /// Keys the just-parsed file carried explicitly. Provenance bookkeeping
    /// only; `load` translates it into `origins`.
    present: BTreeSet<ConfigField>,
    /// `[root] agent`: the template new sessions bind at genesis.
    pub root_agent: Option<String>,
    /// `[roster] working_set = [...]`: the templates spawnable this epoch.
    pub working_set: WorkingSet,
    pub subagents: bool,
    /// `[turn] auto_continue`.
    pub auto_continue: bool,
    /// Workspace writer mode. Internal, not a config key: the product
    /// publishes `single_lease` and nothing else.
    pub occupancy: WriterMode,
    /// `[orchestration]` bounds. A value above the appendix ceiling is a
    /// load error; the snapshot clamp stays on as defense in depth.
    pub max_depth: u16,
    pub max_children: u16,
    pub workspace_idle_seconds: u16,
    pub workspace_budget_mib: u16,
    pub child_retention_days: u16,
    pub child_retention_count: u16,
    pub max_nodes: u16,
    /// Agents one Code Mode cell may spawn. Internal, not a config key:
    /// the default below is clipped to `CEILING_MAX_AGENTS_PER_CELL`
    /// where the budget is published.
    pub max_agents_per_cell: u16,
    /// `[orchestration] max_attempts`: attempts one tree may open in an epoch.
    /// Above the appendix ceiling is a load error; the snapshot clamp stays
    /// on as defense in depth.
    pub max_attempts: u16,
    /// New continuation turns after transient failure, per child attempt.
    pub transient_resumes: u16,
    /// `[orchestration] max_concurrent_requests_per_account`: model requests
    /// one account may have in flight across this process's sessions.
    pub requests_per_account: u16,
    /// `[orchestration] require_review_evidence`: when true, `adopt_candidate`
    /// refuses unless `evidence` names a subtree attempt whose output
    /// references the candidate. Off by default.
    pub require_review_evidence: bool,
    /// `[turn] max_model_rounds`: model rounds one root turn may run before
    /// its tool-free wrap-up round.
    pub max_model_rounds: u16,
    /// `[jobs]` construction-bound foreground budget.
    pub jobs_foreground_seconds: u16,
    /// `[jobs]` construction-bound hard runtime cap.
    pub jobs_max_runtime_hours: u16,
    /// `[api] full_window`: admit new API sessions under the model's full
    /// input window instead of its price threshold.
    pub api_full_window: bool,
    pub tui_mouse: bool,
    /// `[tui] scroll_lines`: transcript lines moved per wheel tick.
    pub scroll_lines: u16,
    /// `[tui] show_thinking`: subject to the provider reasoning policy.
    pub show_thinking: bool,
    /// `[tui] group_tool_calls`: collapse completed transcript activity groups.
    pub group_tool_calls: bool,
    /// `[tui] reduced_motion`: suppress spinner, pulse and camera easing.
    pub reduced_motion: bool,
    pub theme: ProductTheme,
    pub compaction_enabled: bool,
    pub telemetry_max_bytes: u64,
    pub telemetry_max_age_days: u64,
    /// `[tools.<name>]` and `[mcp_servers.<id>]`: options for the optional
    /// tools templates list. A trusted project replaces entries by name.
    pub tool_sections: ToolSections,
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
}

impl PartialEq for ProductConfig {
    fn eq(&self, other: &Self) -> bool {
        self.templates == other.templates
            && self.root_agent == other.root_agent
            && self.working_set == other.working_set
            && self.subagents == other.subagents
            && self.occupancy == other.occupancy
            && self.max_depth == other.max_depth
            && self.max_children == other.max_children
            && self.workspace_idle_seconds == other.workspace_idle_seconds
            && self.workspace_budget_mib == other.workspace_budget_mib
            && self.child_retention_days == other.child_retention_days
            && self.child_retention_count == other.child_retention_count
            && self.max_nodes == other.max_nodes
            && self.max_agents_per_cell == other.max_agents_per_cell
            && self.max_attempts == other.max_attempts
            && self.transient_resumes == other.transient_resumes
            && self.requests_per_account == other.requests_per_account
            && self.require_review_evidence == other.require_review_evidence
            && self.auto_continue == other.auto_continue
            && self.jobs_foreground_seconds == other.jobs_foreground_seconds
            && self.jobs_max_runtime_hours == other.jobs_max_runtime_hours
            && self.api_full_window == other.api_full_window
            && self.max_model_rounds == other.max_model_rounds
            && self.tui_mouse == other.tui_mouse
            && self.scroll_lines == other.scroll_lines
            && self.show_thinking == other.show_thinking
            && self.group_tool_calls == other.group_tool_calls
            && self.reduced_motion == other.reduced_motion
            && self.theme == other.theme
            && self.compaction_enabled == other.compaction_enabled
            && self.telemetry_max_bytes == other.telemetry_max_bytes
            && self.telemetry_max_age_days == other.telemetry_max_age_days
            && self.tool_sections == other.tool_sections
    }
}

impl Eq for ProductConfig {}

/// Every account a `[templates.*]` card may name, and so every account a
/// tree epoch may pin. The product owns this roster; the tree reads it as
/// frozen `bind_hints` and only checks membership.
pub const TEMPLATE_ACCOUNTS: &[&str] = &[
    "codex",
    "chatgpt",
    "grok",
    "supergrok",
    "xai-oauth",
    "claude",
    "openrouter",
    "openai",
    "anthropic",
    "xai-api",
];

/// Accounts with no Codex Code Mode, so a `native-ptc` card on them is a
/// wire-vocabulary error. The `openai` API wears the Codex pack: its cards are
/// checked against the enrolled model table at load instead.
pub const NO_NATIVE_PROTOCOL_ACCOUNTS: &[&str] = &["openrouter", "anthropic", "xai-api"];

fn is_template_account(account: &str) -> bool {
    TEMPLATE_ACCOUNTS.contains(&account)
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ProductTheme {
    #[default]
    DefaultDark,
    DefaultLight,
    CatppuccinLatte,
    CatppuccinFrappe,
    CatppuccinMacchiato,
    CatppuccinMocha,
}

#[derive(Debug, Eq, PartialEq)]
pub enum ConfigSaveError {
    BeforePublication(String),
    PublishedDurabilityUncertain(String),
}

impl ConfigSaveError {
    #[must_use]
    pub fn into_message(self) -> String {
        match self {
            Self::BeforePublication(message) | Self::PublishedDurabilityUncertain(message) => {
                message
            }
        }
    }
}

impl ProductTheme {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DefaultDark => "default-dark",
            Self::DefaultLight => "default-light",
            Self::CatppuccinLatte => "catppuccin-latte",
            Self::CatppuccinFrappe => "catppuccin-frappe",
            Self::CatppuccinMacchiato => "catppuccin-macchiato",
            Self::CatppuccinMocha => "catppuccin-mocha",
        }
    }

    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "default-dark" | "dark" => Ok(Self::DefaultDark),
            // Spec 8.1: `dark` and `light` are aliases of the two default themes.
            "default-light" | "light" => Ok(Self::DefaultLight),
            "catppuccin-latte" => Ok(Self::CatppuccinLatte),
            "catppuccin-frappe" => Ok(Self::CatppuccinFrappe),
            "catppuccin-macchiato" => Ok(Self::CatppuccinMacchiato),
            "catppuccin-mocha" => Ok(Self::CatppuccinMocha),
            _ => Err(format!("unsupported product theme: {value}")),
        }
    }
}

impl ProductConfig {
    /// Resolves the root template's composition only for its complete, exact
    /// account/model binding. An unbound or different target gets the product
    /// default rather than inheriting composition from another tuple.
    pub fn composition_for_tuple(
        &self,
        account: Option<&str>,
        model: Option<&str>,
    ) -> ProductCompositionSelection {
        match (self.root_template(), account, model) {
            (Some(template), Some(account), Some(model))
                if template.account == account && template.model == model =>
            {
                template.composition_or_default()
            }
            _ => ProductCompositionSelection::FollowCatalog,
        }
    }

    /// The effective configuration: `config.toml` plus `~/.plak/agents`, and
    /// — when a trusted project `.plak` is discovered — that project's
    /// `config.toml` key overlay and `agents/` whole-file template
    /// replacements. An absent `config.toml` is the schema default; an absent
    /// agents directory is an empty template set.
    ///
    /// An *untrusted* project applies nothing: the load still succeeds on the
    /// global configuration, `project` names the blocked source and
    /// `warnings` carries the sentence the surfaces show.
    pub fn load(paths: &PlakPaths) -> Result<Self, String> {
        let mut config = Self::load_user(paths)?;
        config.merge_project(paths)?;
        Ok(config)
    }

    /// The global configuration alone, with no project overlay. Every
    /// reference a global file makes must resolve globally — a project
    /// template cannot satisfy a global `root.agent` or `working_set` name —
    /// and save paths diff against this baseline so a write in one scope
    /// cannot flatten the other's values.
    pub fn load_user(paths: &PlakPaths) -> Result<Self, String> {
        let mut config = Self::load_config_file(&paths.config_file())
            .map_err(|error| format!("{error}; run `plak config edit` to fix it"))?;
        for field in config.present.clone() {
            config.origins.insert(field, ConfigValueOrigin::User);
        }
        let (templates, origins) =
            load_templates_dir(&paths.user_agents_dir(), ConfigValueOrigin::User)?;
        config.templates = templates;
        config.template_origins = origins;
        config.validate()?;
        Ok(config)
    }

    fn load_config_file(path: &Path) -> Result<Self, String> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default_product());
            }
            Err(error) => return Err(format!("could not read {}: {error}", path.display())),
        };
        // A load error names the file it came from, exactly as a template
        // load error does.
        Self::parse(&text).map_err(|error| format!("{}: {error}", path.display()))
    }

    /// The trusted-project half of `load`: nothing when there is no project
    /// home, nothing but the named warning when the project is untrusted, and
    /// the key overlay plus whole-template replacements when it is.
    fn merge_project(&mut self, paths: &PlakPaths) -> Result<(), String> {
        let Some(home) = paths.project_home() else {
            return Ok(());
        };
        // Launching inside the product home's parent would otherwise read
        // `~/.plak` as its own "project" override. Both sides canonicalize so
        // an aliased PLAK_HOME still answers true.
        if fs::canonicalize(&home).ok().as_deref() == fs::canonicalize(paths.home()).ok().as_deref()
            || home == paths.home()
        {
            return Ok(());
        }
        let config_path = home.join("config.toml");
        let agents_dir = home.join("agents");
        let has_config = config_path.is_file();
        let has_agents = agents_dir.is_dir();
        if !has_config && !has_agents {
            return Ok(());
        }
        let Some(root) = paths
            .project_root()
            .map(|root| fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf()))
        else {
            return Ok(());
        };
        if !project_trust::is_trusted(paths, &root) {
            let mut sources = Vec::new();
            if has_config {
                sources.push("config.toml");
            }
            if has_agents {
                sources.push("agents/");
            }
            self.project = Some(ProjectConfigSource {
                root,
                trusted: false,
            });
            self.warnings.push(format!(
                "project {} is not trusted; its {} not applied (run `plak config trust` to enable it)",
                home.display(),
                sources.join(" and ")
            ));
            return Ok(());
        }
        if has_config {
            let overlay = Self::load_config_file(&config_path)
                .map_err(|error| format!("{error}; edit this file to fix it"))?;
            if overlay.present.iter().any(|field| field.is_home_only()) {
                self.warnings.push(format!(
                    "{}: [orchestration] is home-scoped and ignored here; set it in {}",
                    config_path.display(),
                    paths.config_file().display(),
                ));
            }
            for field in overlay.present.clone() {
                if field.is_home_only() {
                    continue;
                }
                self.assign_overlay_field(field, &overlay);
                self.origins.insert(field, ConfigValueOrigin::Project);
            }
            self.tool_sections.overlay(&overlay.tool_sections);
        }
        if has_agents {
            let (templates, origins) = load_templates_dir(&agents_dir, ConfigValueOrigin::Project)?;
            for (name, template) in templates {
                self.templates.insert(name, template);
            }
            self.template_origins.extend(origins);
        }
        self.project = Some(ProjectConfigSource {
            root,
            trusted: true,
        });
        // The global half already validated; a merged failure is something
        // the project asked for, so the error names its document.
        self.validate().map_err(|error| {
            let source = if has_config {
                &config_path
            } else {
                &agents_dir
            };
            format!("{}: {error}", source.display())
        })
    }

    /// One key's value copied from a same-shaped config — the project
    /// overlay's contribution, or the baseline restore on a user-scope save.
    /// The match is exhaustive so a new `ConfigField` cannot merge silently.
    fn assign_overlay_field(&mut self, field: ConfigField, source: &Self) {
        match field {
            ConfigField::RootAgent => self.root_agent = source.root_agent.clone(),
            ConfigField::WorkingSet => self.working_set = source.working_set.clone(),
            ConfigField::Subagents => self.subagents = source.subagents,
            ConfigField::AutoContinue => {
                self.auto_continue = source.auto_continue;
            }
            ConfigField::MaxDepth => self.max_depth = source.max_depth,
            ConfigField::MaxChildren => self.max_children = source.max_children,
            ConfigField::WorkspaceIdleSeconds => {
                self.workspace_idle_seconds = source.workspace_idle_seconds;
            }
            ConfigField::WorkspaceBudgetMib => {
                self.workspace_budget_mib = source.workspace_budget_mib;
            }
            ConfigField::ChildRetentionDays => {
                self.child_retention_days = source.child_retention_days;
            }
            ConfigField::ChildRetentionCount => {
                self.child_retention_count = source.child_retention_count;
            }
            ConfigField::MaxNodes => self.max_nodes = source.max_nodes,
            ConfigField::MaxAttempts => self.max_attempts = source.max_attempts,
            ConfigField::TransientResumes => self.transient_resumes = source.transient_resumes,
            ConfigField::RequestsPerAccount => {
                self.requests_per_account = source.requests_per_account;
            }
            ConfigField::RequireReviewEvidence => {
                self.require_review_evidence = source.require_review_evidence;
            }
            ConfigField::MaxModelRounds => self.max_model_rounds = source.max_model_rounds,
            ConfigField::JobsForegroundSeconds => {
                self.jobs_foreground_seconds = source.jobs_foreground_seconds;
            }
            ConfigField::JobsMaxRuntimeHours => {
                self.jobs_max_runtime_hours = source.jobs_max_runtime_hours;
            }
            ConfigField::ApiFullWindow => self.api_full_window = source.api_full_window,
            ConfigField::TuiMouse => self.tui_mouse = source.tui_mouse,
            ConfigField::Theme => self.theme = source.theme,
            ConfigField::ScrollLines => self.scroll_lines = source.scroll_lines,
            ConfigField::ShowThinking => self.show_thinking = source.show_thinking,
            ConfigField::GroupToolCalls => self.group_tool_calls = source.group_tool_calls,
            ConfigField::ReducedMotion => self.reduced_motion = source.reduced_motion,
            ConfigField::CompactionEnabled => {
                self.compaction_enabled = source.compaction_enabled;
            }
            ConfigField::TelemetryMaxBytes => {
                self.telemetry_max_bytes = source.telemetry_max_bytes;
            }
            ConfigField::TelemetryMaxAgeDays => {
                self.telemetry_max_age_days = source.telemetry_max_age_days;
            }
        }
    }

    /// The file behind `paths`, or the schema defaults when it cannot be read.
    ///
    /// The typed slices are all produced through this one loader, so "an
    /// unreadable config is the schema default" is decided once.
    #[must_use]
    pub fn load_or_default(paths: &PlakPaths) -> Self {
        Self::load(paths).unwrap_or_else(|_| Self::default_product())
    }

    /// Validated process limits for new sessions and explicit rebinds.
    pub fn job_config(&self) -> Result<plakous::tools::JobConfig, String> {
        plakous::tools::JobConfig::new(
            u64::from(self.jobs_foreground_seconds),
            u64::from(self.jobs_max_runtime_hours),
        )
        .map_err(|error| error.to_string())
    }

    /// The hosted-session policy slice ([`SessionPolicy`]).
    #[must_use]
    pub fn session_policy(&self) -> SessionPolicy {
        SessionPolicy {
            compaction_enabled: self.compaction_enabled,
            max_model_rounds: usize::from(self.max_model_rounds),
            jobs_foreground_seconds: self.jobs_foreground_seconds,
            jobs_max_runtime_hours: self.jobs_max_runtime_hours,
            api_full_window: self.api_full_window,
            topology_enabled: self.subagents,
            auto_continue: self.auto_continue,
            requests_per_account: self.requests_per_account,
            root_template: self.root_template_fact(),
        }
    }

    /// The telemetry retention slice ([`TelemetryConfig`]).
    #[must_use]
    pub const fn telemetry(&self) -> TelemetryConfig {
        TelemetryConfig {
            max_bytes: self.telemetry_max_bytes,
            max_age_days: self.telemetry_max_age_days,
        }
    }

    /// The `[root]` binding slice ([`DefaultBinding`]).
    #[must_use]
    pub fn default_binding(&self) -> DefaultBinding {
        DefaultBinding {
            account: self.default_account().map(str::to_owned),
            model: self.default_model().map(str::to_owned),
            effort: self.default_effort().map(str::to_owned),
            extra: self.default_extra().map(str::to_owned),
            composition: self.default_composition(),
            protocol: self.default_protocol(),
        }
    }

    #[must_use]
    pub fn default_product() -> Self {
        Self {
            templates: BTreeMap::new(),
            template_origins: BTreeMap::new(),
            origins: BTreeMap::new(),
            project: None,
            warnings: Vec::new(),
            present: BTreeSet::new(),
            root_agent: None,
            working_set: WorkingSet::default(),
            subagents: true,
            occupancy: WriterMode::SingleLease,
            max_depth: 1,
            max_children: DEFAULT_DIRECT_CHILDREN as u16,
            workspace_idle_seconds: 600,
            workspace_budget_mib: 4096,
            child_retention_days: 7,
            child_retention_count: 128,
            max_nodes: DEFAULT_TREE_NODES as u16,
            max_agents_per_cell: plakous_hosted::cell_budget::DEFAULT_MAX_AGENTS_PER_CELL,
            max_attempts: DEFAULT_TREE_ATTEMPTS as u16,
            transient_resumes: 1,
            requests_per_account: 8,
            require_review_evidence: false,
            auto_continue: true,
            max_model_rounds: DEFAULT_MAX_MODEL_ROUNDS,
            jobs_foreground_seconds: 30,
            jobs_max_runtime_hours: 10,
            api_full_window: false,
            tui_mouse: true,
            scroll_lines: DEFAULT_SCROLL_LINES,
            show_thinking: true,
            group_tool_calls: true,
            reduced_motion: false,
            theme: ProductTheme::DefaultDark,
            compaction_enabled: true,
            telemetry_max_bytes: crate::telemetry_store::DEFAULT_TELEMETRY_MAX_BYTES,
            telemetry_max_age_days: crate::telemetry_store::DEFAULT_TELEMETRY_MAX_AGE_DAYS,
            tool_sections: ToolSections::default(),
        }
    }

    /// The `[root] agent` template, when one is configured and present.
    #[must_use]
    pub fn root_template(&self) -> Option<&AgentTemplate> {
        self.root_agent
            .as_deref()
            .and_then(|name| self.templates.get(name))
    }

    #[must_use]
    pub fn default_account(&self) -> Option<&str> {
        self.root_template()
            .map(|template| template.account.as_str())
    }

    #[must_use]
    pub fn default_model(&self) -> Option<&str> {
        self.root_template().map(|template| template.model.as_str())
    }

    #[must_use]
    pub fn default_effort(&self) -> Option<&str> {
        self.root_template()
            .and_then(|template| template.effort.as_deref())
    }

    #[must_use]
    pub fn default_extra(&self) -> Option<&str> {
        self.root_template()
            .and_then(|template| template.extra.as_deref())
    }

    #[must_use]
    pub fn default_composition(&self) -> ProductCompositionSelection {
        self.root_template()
            .map_or(ProductCompositionSelection::FollowCatalog, |template| {
                template.composition_or_default()
            })
    }

    #[must_use]
    pub fn default_protocol(&self) -> ProductProtocolSelection {
        self.root_template()
            .map_or(ProductProtocolSelection::Auto, |template| {
                template.protocol_or_default()
            })
    }

    /// Template named in the working set this file (not this epoch: bind
    /// authority is the frozen snapshot).
    #[must_use]
    pub fn working_set_template(&self, name: &str) -> Option<&AgentTemplate> {
        self.working_set
            .contains(name)
            .then(|| self.templates.get(name))
            .flatten()
    }

    /// The `[root] agent` template as a fact a hosted session can compare its
    /// own bound tuple against without reading the file again.
    #[must_use]
    pub fn root_template_fact(&self) -> Option<RootTemplateFact> {
        let name = self.root_agent.clone()?;
        let template = self.templates.get(&name)?;
        Some(RootTemplateFact {
            name,
            account: template.account.clone(),
            model: template.model.clone(),
            tools: template.tools.clone(),
            tool_sections: self.tool_sections.reached(&template.tools),
        })
    }

    /// Copy the current root into a fresh, unshared template before editing.
    /// The active file stays immutable until the config pointer is published.
    pub fn root_template_mut(&mut self) -> &mut AgentTemplate {
        let body = self.root_template().cloned().unwrap_or_default();
        let minted = self.mint_unshared_root_template_name();
        self.templates.insert(minted.clone(), body);
        self.root_agent = Some(minted.clone());
        self.templates.entry(minted).or_default()
    }

    /// First free `default`, `default-2`, … name not already a template and not
    /// named by the working set.
    fn mint_unshared_root_template_name(&self) -> String {
        let taken =
            |name: &str| self.templates.contains_key(name) || self.working_set.contains(name);
        if !taken(MINTED_ROOT_TEMPLATE_NAME) {
            return MINTED_ROOT_TEMPLATE_NAME.to_owned();
        }
        for suffix in 2_u32..=u32::MAX {
            let candidate = format!("{MINTED_ROOT_TEMPLATE_NAME}-{suffix}");
            if !taken(&candidate) {
                return candidate;
            }
        }
        MINTED_ROOT_TEMPLATE_NAME.to_owned()
    }

    /// Points `[root] agent` at one fresh template carrying this exact tuple,
    /// returning the same body so a caller can set `extra` / `composition`.
    pub fn bind_root_tuple(
        &mut self,
        account: &str,
        model: &str,
        effort: Option<&str>,
    ) -> &mut AgentTemplate {
        let template = self.root_template_mut();
        template.account = account.to_owned();
        template.model = model.to_owned();
        template.effort = effort.map(str::to_owned);
        template
    }

    /// Drops the root bind. Every template keeps its file — templates are
    /// file-authored and `save` never deletes one — so only the `[root]`
    /// pointer goes; a partial tuple is not an expressible template.
    pub fn clear_root_binding(&mut self) {
        self.root_agent = None;
    }

    /// Readiness for every template, in name order.
    #[must_use]
    pub fn readiness_rows(
        &self,
        catalog: &[plakous_host_protocol::views::ProviderSection],
    ) -> Vec<TemplateReadinessRow> {
        self.templates
            .iter()
            .map(|(name, template)| TemplateReadinessRow {
                name: name.clone(),
                template: template.clone(),
                readiness: template_readiness(
                    template,
                    catalog_credential_readiness(&template.account, catalog),
                    catalog,
                ),
            })
            .collect()
    }

    /// `config.toml` as typed TOML: document, schema, then semantics.
    ///
    /// Templates are not in this file, so name resolution (`[root] agent`,
    /// `[roster] working_set`) is checked by [`Self::validate`] after
    /// `load_templates`, not here.
    pub fn parse(text: &str) -> Result<Self, String> {
        toml_file::parse_config(text)
    }

    /// The main config only; templates render one file each with
    /// `render_template`.
    ///
    /// Section order, key order and value spelling all come from
    /// [`CONFIG_KEYS`], the one table [`Self::render_annotated`] and the
    /// spec's config reference also read.
    #[must_use]
    pub fn render(&self) -> String {
        self.render_with(false)
    }

    /// The same file with the reference commentary attached: one meaning line
    /// and one type/default/constraint line above every key, and a commented
    /// example for a key this config leaves unset.
    ///
    /// `configs/reference/config.toml` is exactly this output for the schema
    /// defaults, regenerated by a test, so the shipped reference cannot drift
    /// from the parser.
    #[must_use]
    pub fn render_annotated(&self) -> String {
        self.render_with(true)
    }

    fn render_with(&self, annotated: bool) -> String {
        let mut out = String::from("# Plak product config (non-secret)\n");
        if annotated {
            out.push_str(
                "#\n# Every key the parser accepts, with its type, default and constraint.\n\
                 # This is TOML. Deleting a key restores its default; an unknown key or\n\
                 # section, a duplicate key, a wrong type and malformed syntax are all\n\
                 # named load errors, never a silent skip.\n",
            );
        }
        for (section, purpose) in CONFIG_SECTION_DOCS {
            out.push('\n');
            if annotated {
                out.push_str(&format!("# {purpose}\n"));
            }
            out.push_str(&format!("[{section}]\n"));
            for row in CONFIG_KEYS.iter().filter(|row| row.section() == *section) {
                if annotated {
                    out.push_str(&row.comment_block());
                }
                match self.value_line(row) {
                    Some(line) => out.push_str(&line),
                    None if annotated => {
                        out.push_str(&format!("# {} = {}\n", row.key(), row.example));
                    }
                    None => {}
                }
            }
        }
        if annotated {
            out.push_str(TOOL_SECTIONS_REFERENCE);
        }
        out
    }

    /// The rendered `key = value` line, or `None` for a key this config leaves
    /// unset (only `[root] agent` can be unset).
    fn value_line(&self, key: &ConfigKey) -> Option<String> {
        Some(format!(
            "{} = {}\n",
            key.key(),
            self.field_value(key.field)?
        ))
    }

    /// One key's value as `config.toml` carries it, or `None` for a key this
    /// config leaves unset (only `[root] agent` can be unset).
    ///
    /// One exhaustive match over [`ConfigField`]: a new key cannot render
    /// without the compiler asking for its value. The renderer, the `default:`
    /// comment in the reference file and the surgical editor behind a save all
    /// read this one answer, so none of them can spell a value differently.
    fn field_value(&self, field: ConfigField) -> Option<Value> {
        Some(match field {
            ConfigField::RootAgent => Value::from(self.root_agent.as_deref()?),
            ConfigField::WorkingSet => Value::Array(
                self.working_set
                    .names()
                    .iter()
                    .map(|name| Value::from(name.as_str()))
                    .collect(),
            ),
            ConfigField::Subagents => Value::from(self.subagents),
            ConfigField::AutoContinue => Value::from(self.auto_continue),
            ConfigField::MaxDepth => Value::from(i64::from(self.max_depth)),
            ConfigField::MaxChildren => Value::from(i64::from(self.max_children)),
            ConfigField::WorkspaceIdleSeconds => {
                Value::from(i64::from(self.workspace_idle_seconds))
            }
            ConfigField::WorkspaceBudgetMib => Value::from(i64::from(self.workspace_budget_mib)),
            ConfigField::ChildRetentionDays => Value::from(i64::from(self.child_retention_days)),
            ConfigField::ChildRetentionCount => Value::from(i64::from(self.child_retention_count)),
            ConfigField::MaxNodes => Value::from(i64::from(self.max_nodes)),
            ConfigField::MaxAttempts => Value::from(i64::from(self.max_attempts)),
            ConfigField::TransientResumes => Value::from(i64::from(self.transient_resumes)),
            ConfigField::RequestsPerAccount => Value::from(i64::from(self.requests_per_account)),
            ConfigField::RequireReviewEvidence => Value::from(self.require_review_evidence),
            ConfigField::MaxModelRounds => Value::from(i64::from(self.max_model_rounds)),
            ConfigField::JobsForegroundSeconds => {
                Value::from(i64::from(self.jobs_foreground_seconds))
            }
            ConfigField::JobsMaxRuntimeHours => Value::from(i64::from(self.jobs_max_runtime_hours)),
            ConfigField::ApiFullWindow => Value::from(self.api_full_window),
            ConfigField::TuiMouse => Value::from(self.tui_mouse),
            ConfigField::Theme => Value::from(self.theme.as_str()),
            ConfigField::ScrollLines => Value::from(i64::from(self.scroll_lines)),
            ConfigField::ShowThinking => Value::from(self.show_thinking),
            ConfigField::GroupToolCalls => Value::from(self.group_tool_calls),
            ConfigField::ReducedMotion => Value::from(self.reduced_motion),
            ConfigField::CompactionEnabled => Value::from(self.compaction_enabled),
            // A TOML integer is an `i64`; the two retention bounds are held as
            // `u64` for the store and can only have come from one.
            ConfigField::TelemetryMaxBytes => {
                Value::from(saturating_toml_integer(self.telemetry_max_bytes))
            }
            ConfigField::TelemetryMaxAgeDays => {
                Value::from(saturating_toml_integer(self.telemetry_max_age_days))
            }
        })
    }

    /// Which file decided this key's effective value.
    #[must_use]
    pub fn origin(&self, field: ConfigField) -> ConfigValueOrigin {
        self.origins
            .get(&field)
            .copied()
            .unwrap_or(ConfigValueOrigin::Default)
    }

    /// Which file this template's effective body came from.
    #[must_use]
    pub fn template_origin(&self, name: &str) -> ConfigValueOrigin {
        self.template_origins
            .get(name)
            .copied()
            .unwrap_or(ConfigValueOrigin::User)
    }

    /// Records the scope a caller intends to publish this key under. Save
    /// paths re-derive what each file needs; the origin is provenance for
    /// display and for the user-scope refusal in `config set`.
    pub fn set_origin(&mut self, field: ConfigField, origin: ConfigValueOrigin) {
        self.origins.insert(field, origin);
    }

    pub fn save(&self, paths: &PlakPaths) -> Result<(), String> {
        self.save_publication_aware(paths)
            .map_err(ConfigSaveError::into_message)
    }

    /// A save with the destination named. `User` publishes only values and
    /// templates the project did not supply; `Project` publishes only the
    /// delta from the global baseline into `.plak/`. Neither scope writes the
    /// other's files, and neither flattens the merged view into one file.
    pub fn save_scoped(
        &self,
        paths: &PlakPaths,
        scope: ConfigWriteScope,
    ) -> Result<(), ConfigSaveError> {
        match scope {
            ConfigWriteScope::User => self.save_publication_aware(paths),
            ConfigWriteScope::Project => self.save_project_publication_aware(paths),
        }
    }

    /// User-scope save. When project sources contributed to this merged
    /// config, the global file still receives only global content: each
    /// project-origin key keeps its baseline value and project templates are
    /// not written into `~/.plak/agents`.
    pub fn save_publication_aware(&self, paths: &PlakPaths) -> Result<(), ConfigSaveError> {
        let mut user_view = self.clone();
        if self
            .origins
            .values()
            .chain(self.template_origins.values())
            .any(|origin| *origin == ConfigValueOrigin::Project)
        {
            let baseline = Self::load_user(paths).map_err(ConfigSaveError::BeforePublication)?;
            for (field, origin) in &self.origins {
                if *origin == ConfigValueOrigin::Project {
                    user_view.assign_overlay_field(*field, &baseline);
                }
            }
            user_view
                .templates
                .retain(|name, _| self.template_origin(name) != ConfigValueOrigin::Project);
            user_view
                .template_origins
                .retain(|_, origin| *origin != ConfigValueOrigin::Project);
        }
        user_view.save_with_fault(paths, &mut |_, _| Ok(()))
    }

    /// Project-scope save: `<project>/.plak/config.toml` carries only the
    /// keys whose effective value differs from the global baseline, and
    /// `.plak/agents/` gains only the templates that differ from (or are
    /// absent from) the user directory. Requires a trusted project.
    pub fn save_project_publication_aware(&self, paths: &PlakPaths) -> Result<(), ConfigSaveError> {
        self.save_project_with_fault(paths, &mut |_, _| Ok(()))
    }

    fn save_project_with_fault(
        &self,
        paths: &PlakPaths,
        fault: &mut impl FnMut(&Path, AtomicStage) -> Result<(), String>,
    ) -> Result<(), ConfigSaveError> {
        self.validate()
            .map_err(ConfigSaveError::BeforePublication)?;
        let Some(root) = paths.project_root().map(Path::to_path_buf) else {
            return Err(ConfigSaveError::BeforePublication(
                "no project .plak was discovered from the current directory; create .plak/config.toml or .plak/agents/ first".to_owned(),
            ));
        };
        let home = root.join(".plak");
        if home == paths.home() {
            return Err(ConfigSaveError::BeforePublication(
                "the product home is not a project configuration override".to_owned(),
            ));
        }
        if !project_trust::is_trusted(paths, &root) {
            return Err(ConfigSaveError::BeforePublication(format!(
                "project {} is not trusted; run `plak config trust` before writing project configuration",
                root.display()
            )));
        }
        let baseline = Self::load_user(paths).map_err(ConfigSaveError::BeforePublication)?;
        for row in CONFIG_KEYS.iter().filter(|row| row.field.is_home_only()) {
            if !toml_file::same_value_opt(
                self.field_value(row.field).as_ref(),
                baseline.field_value(row.field).as_ref(),
            ) {
                return Err(ConfigSaveError::BeforePublication(format!(
                    "{} is home-scoped; use --scope user to change orchestration authority",
                    row.path
                )));
            }
        }
        let agents_dir = home.join("agents");
        let mut fresh = Vec::new();
        for (name, template) in &self.templates {
            if baseline.templates.get(name) == Some(template) {
                continue;
            }
            let target = agents_dir.join(format!("{name}.toml"));
            match fs::read_to_string(&target) {
                Ok(body) => {
                    if parse_template(&body, name).map_err(ConfigSaveError::BeforePublication)?
                        != *template
                    {
                        return Err(ConfigSaveError::BeforePublication(format!(
                            "template {name} is defined by the project ({}); edit it there",
                            target.display()
                        )));
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    fresh.push((target, render_template(template)));
                }
                Err(_) => {
                    return Err(ConfigSaveError::BeforePublication(format!(
                        "could not read {}",
                        target.display()
                    )));
                }
            }
        }
        let config_path = home.join("config.toml");
        let existing = match fs::read_to_string(&config_path) {
            Ok(text) => Some(text),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(_) => {
                return Err(ConfigSaveError::BeforePublication(format!(
                    "could not read {}",
                    config_path.display()
                )));
            }
        };
        let has_overrides = CONFIG_KEYS.iter().any(|row| {
            !toml_file::same_value_opt(
                self.field_value(row.field).as_ref(),
                baseline.field_value(row.field).as_ref(),
            )
        });
        let published = if existing.is_some() || has_overrides {
            Some(
                toml_file::edit_project_document(existing.as_deref(), self, &baseline)
                    .map_err(ConfigSaveError::BeforePublication)?,
            )
        } else {
            None
        };
        if !fresh.is_empty() {
            fs::create_dir_all(&agents_dir).map_err(|_| {
                ConfigSaveError::BeforePublication(format!(
                    "could not create {}",
                    agents_dir.display()
                ))
            })?;
        }
        for (target, rendered) in fresh {
            atomic_file(&target, rendered.as_bytes(), true, fault)?;
        }
        if let Some(published) = published {
            fs::create_dir_all(&home).map_err(|_| {
                ConfigSaveError::BeforePublication(format!("could not create {}", home.display()))
            })?;
            atomic_file(&config_path, published.as_bytes(), false, fault)?;
        }
        Ok(())
    }

    fn save_with_fault(
        &self,
        paths: &PlakPaths,
        fault: &mut impl FnMut(&Path, AtomicStage) -> Result<(), String>,
    ) -> Result<(), ConfigSaveError> {
        self.validate()
            .map_err(ConfigSaveError::BeforePublication)?;
        paths
            .ensure_layout()
            .map_err(ConfigSaveError::BeforePublication)?;
        self.save_templates_with_fault(paths, fault)
            .map_err(ConfigSaveError::BeforePublication)?;
        // A newly created agents directory is itself a prerequisite entry in
        // the home directory, not just the parent of its template files.
        sync_parent_directory(paths.home()).map_err(ConfigSaveError::BeforePublication)?;
        // Surgical: the file already on disk keeps its comments, its key order
        // and its spacing, and only the keys whose value actually changed are
        // rewritten. A home with no `config.toml` yet gets the canonical
        // render, which is what `plak config init` writes.
        let path = paths.config_file();
        let existing = match fs::read_to_string(&path) {
            Ok(text) => Some(text),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(ConfigSaveError::BeforePublication(format!(
                    "could not read {} before saving: {error}",
                    path.display()
                )));
            }
        };
        let published = toml_file::edit_document(existing.as_deref(), self)
            .map_err(ConfigSaveError::BeforePublication)?;
        atomic_file(&path, published.as_bytes(), false, fault)
    }

    /// The one rule set: load, `plak config set`, and `SaveRoster` share it.
    pub fn validate(&self) -> Result<(), String> {
        self.job_config()?;
        for (name, template) in &self.templates {
            template.validate(name)?;
            for child in &template.spawns {
                let Some(record) = self.templates.get(child.as_str()) else {
                    return Err(format!(
                        "template {name} spawns {child}, and no template file {child}.toml exists under ~/.plak/agents or .plak/agents"
                    ));
                };
                if record.description.trim().is_empty() {
                    return Err(format!(
                        "template {child} is in template {name}'s spawns and needs a description for its spawn card"
                    ));
                }
            }
        }
        if let Some(agent) = self.root_agent.as_deref()
            && !self.templates.contains_key(agent)
        {
            return Err(format!(
                "[root] agent names no template file {agent}.toml under ~/.plak/agents or .plak/agents"
            ));
        }
        for name in self.working_set.names() {
            let Some(record) = self.templates.get(name.as_str()) else {
                return Err(format!(
                    "[roster] working_set names no template file {name}.toml under ~/.plak/agents or .plak/agents"
                ));
            };
            if record.description.trim().is_empty() {
                return Err(format!(
                    "[roster] working_set template {name} needs a description for its spawn card"
                ));
            }
        }
        self.validate_tool_references()
    }

    /// Every `mcp:<id>` a template lists names an `[mcp_servers.<id>]`
    /// table, and an `mcp:<id>/<tool>` names a tool that table allows.
    fn validate_tool_references(&self) -> Result<(), String> {
        for (name, template) in &self.templates {
            for entry in template.tools.entries() {
                let (server, tool) = match entry {
                    SelectedTool::Optional(_) => continue,
                    SelectedTool::McpServer(server) => (server.as_str(), None),
                    SelectedTool::McpTool { server, tool } => (server.as_str(), Some(tool)),
                };
                let Some(config) = self.tool_sections.mcp_servers.get(server) else {
                    return Err(format!(
                        "template {name}: tools names {entry}, and config.toml has no [mcp_servers.{server}]"
                    ));
                };
                if let (Some(tool), Some(allowed)) = (tool, &config.tools)
                    && !allowed.contains(tool)
                {
                    return Err(format!(
                        "template {name}: tools names {entry}, which [mcp_servers.{server}] tools does not allow"
                    ));
                }
            }
        }
        Ok(())
    }

    /// Preflight every existing body before creating any file. Existing files
    /// are user-authored, never rewritten as a side effect of settings saves.
    #[cfg(test)]
    fn save_templates(&self, paths: &PlakPaths) -> Result<(), String> {
        paths.ensure_layout()?;
        self.save_templates_with_fault(paths, &mut |_, _| Ok(()))
    }

    /// User-scope template staging: `~/.plak/agents` only. Project-owned
    /// bodies are filtered out by `save_publication_aware` before this runs;
    /// a surviving project-origin entry is skipped again here so the home
    /// cannot absorb it even on a caller mistake.
    fn save_templates_with_fault(
        &self,
        paths: &PlakPaths,
        fault: &mut impl FnMut(&Path, AtomicStage) -> Result<(), String>,
    ) -> Result<(), String> {
        let user_dir = paths.user_agents_dir();
        let mut fresh = Vec::new();
        for (name, template) in &self.templates {
            if self.template_origin(name) == ConfigValueOrigin::Project {
                continue;
            }
            let target = user_dir.join(format!("{name}.toml"));
            match fs::read_to_string(&target) {
                Ok(body) => {
                    if parse_template(&body, name)? != *template {
                        return Err(format!(
                            "template {name} already exists ({}); edit it there",
                            target.display()
                        ));
                    }
                    OpenOptions::new()
                        .read(true)
                        .open(&target)
                        .and_then(|file| file.sync_all())
                        .map_err(|_| format!("could not sync {}", target.display()))?;
                    sync_parent_directory(target.parent().unwrap_or(&user_dir))?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    fresh.push((target, render_template(template)));
                }
                Err(_) => return Err(format!("could not read {}", target.display())),
            }
        }
        for (target, rendered) in fresh {
            // These names are absent from the current config. A failed staging
            // operation may leave an orphan, but has not published the pointer.
            atomic_file(&target, rendered.as_bytes(), true, fault)
                .map_err(ConfigSaveError::into_message)?;
        }
        // This also closes a retry after a prior template directory-sync
        // failure left the new body visible but not durably named.
        sync_parent_directory(&user_dir)?;
        Ok(())
    }

    /// The closed `plak config set` key list. Template bodies are
    /// not settable: a template is authored in its own file. A
    /// rejected key leaves the config untouched: the whole rule set runs on a
    /// candidate copy.
    pub fn set_key(&mut self, key: &str, value: &str) -> Result<(), String> {
        let mut candidate = self.clone();
        candidate.assign_key(key, value)?;
        candidate.validate()?;
        *self = candidate;
        Ok(())
    }

    fn assign_key(&mut self, key: &str, value: &str) -> Result<(), String> {
        let replacement = match key {
            "mouse" => Some("tui.mouse"),
            "theme" => Some("tui.theme"),
            "compaction_enabled" => Some("compaction.enabled"),
            _ => None,
        };
        if let Some(replacement) = replacement {
            return Err(format!("unknown config key: {key}; use {replacement}"));
        }
        if let Some(row) = config_key(key) {
            return self.assign_field(row, FieldValue::Cli(value));
        }
        if key.starts_with("agent.") {
            let name = key
                .trim_start_matches("agent.")
                .split('.')
                .next()
                .unwrap_or_default();
            return Err(format!(
                "templates are file-authored; edit ~/.plak/agents/{name}.toml"
            ));
        }
        Err(format!("unknown config key: {key}"))
    }

    /// Writes one documented key, whatever spelled it.
    ///
    /// This is the product's one semantic layer: a `config.toml` value and a
    /// `plak config set` argument reach it through the same door, so the file
    /// and the command cannot disagree about what a value means. The document
    /// layer has already settled syntax and duplicates; what is left here is
    /// the type each key takes and the values it admits.
    fn assign_field(&mut self, key: &ConfigKey, value: FieldValue<'_>) -> Result<(), String> {
        match key.field {
            ConfigField::RootAgent => {
                // Empty clears the pointer, the same spelling that empties the
                // working set; a partial tuple is not an expressible template.
                // In a file an empty name is a named error, not a clear.
                let name = value.string(key)?;
                if matches!(value, FieldValue::Cli(_)) && name.is_empty() {
                    self.clear_root_binding();
                } else {
                    validate_template_name(name)?;
                    self.root_agent = Some(name.to_owned());
                }
            }
            ConfigField::WorkingSet => self.working_set = WorkingSet::parse(value.names()?)?,
            ConfigField::Subagents => self.subagents = value.boolean()?,
            ConfigField::AutoContinue => self.auto_continue = value.boolean()?,
            ConfigField::MaxDepth => {
                self.max_depth = parse_bound(key.key(), value, CEILING_MAX_DEPTH)?;
            }
            ConfigField::MaxChildren => {
                self.max_children = parse_bound(key.key(), value, CEILING_MAX_CHILDREN)?;
            }
            ConfigField::WorkspaceIdleSeconds => {
                self.workspace_idle_seconds = parse_bound(key.key(), value, u16::MAX)?;
            }
            ConfigField::WorkspaceBudgetMib => {
                self.workspace_budget_mib = parse_bound(key.key(), value, u16::MAX)?;
            }
            ConfigField::ChildRetentionDays => {
                self.child_retention_days = parse_bound(key.key(), value, u16::MAX)?;
            }
            ConfigField::ChildRetentionCount => {
                self.child_retention_count = parse_bound(key.key(), value, u16::MAX)?;
            }
            ConfigField::MaxNodes => {
                self.max_nodes = parse_bound(key.key(), value, CEILING_MAX_NODES)?;
            }
            ConfigField::MaxAttempts => {
                self.max_attempts = parse_bound(key.key(), value, CEILING_MAX_ATTEMPTS)?;
            }
            ConfigField::TransientResumes => {
                let count = value
                    .integer()
                    .ok_or_else(|| format!("{} must be 0..=2", key.key()))?;
                if !(0..=2).contains(&count) {
                    return Err(format!("{} must be 0..=2", key.key()));
                }
                self.transient_resumes =
                    u16::try_from(count).map_err(|_| format!("{} must be 0..=2", key.key()))?;
            }
            ConfigField::RequestsPerAccount => {
                self.requests_per_account =
                    parse_bound(key.key(), value, CEILING_REQUESTS_PER_ACCOUNT)?;
            }
            ConfigField::RequireReviewEvidence => self.require_review_evidence = value.boolean()?,
            ConfigField::MaxModelRounds => {
                self.max_model_rounds = parse_max_model_rounds(value)?;
            }
            ConfigField::JobsForegroundSeconds => {
                self.jobs_foreground_seconds = parse_bound(key.path, value, 600)?;
            }
            ConfigField::JobsMaxRuntimeHours => {
                self.jobs_max_runtime_hours = parse_bound(key.path, value, 24)?;
            }
            ConfigField::ApiFullWindow => self.api_full_window = value.boolean()?,
            ConfigField::TuiMouse => self.tui_mouse = value.boolean()?,
            ConfigField::Theme => self.theme = ProductTheme::parse(value.string(key)?)?,
            ConfigField::ScrollLines => self.scroll_lines = parse_scroll_lines(value)?,
            ConfigField::ShowThinking => self.show_thinking = value.boolean()?,
            ConfigField::GroupToolCalls => self.group_tool_calls = value.boolean()?,
            ConfigField::ReducedMotion => self.reduced_motion = value.boolean()?,
            ConfigField::CompactionEnabled => self.compaction_enabled = value.boolean()?,
            ConfigField::TelemetryMaxBytes => {
                self.telemetry_max_bytes = parse_positive(key.path, value)?;
            }
            ConfigField::TelemetryMaxAgeDays => {
                self.telemetry_max_age_days = parse_positive(key.path, value)?;
            }
        }
        Ok(())
    }

    /// The effective config with its provenance: which files participated,
    /// any blocked source, and where every key and template was decided.
    #[must_use]
    pub fn display_redacted(&self, paths: &PlakPaths) -> String {
        let mut out = format!("config_path={}\n", paths.config_file().display());
        match &self.project {
            Some(source) => out.push_str(&format!(
                "project={} ({})\n",
                source.root.join(".plak").display(),
                if source.trusted {
                    "trusted"
                } else {
                    "untrusted — ignored"
                }
            )),
            None => out.push_str("project=none\n"),
        }
        for warning in &self.warnings {
            out.push_str(&format!("warning={warning}\n"));
        }
        out.push_str(&self.render());
        out.push_str(&self.tool_sections.render());
        out.push_str("\n# value origins (which file decided each key)\n");
        for row in CONFIG_KEYS {
            out.push_str(&format!(
                "origin.{} = {}\n",
                row.path,
                self.origin(row.field).as_str()
            ));
        }
        for name in self.templates.keys() {
            out.push_str(&format!(
                "origin.template.{name} = {}\n",
                self.template_origin(name).as_str()
            ));
        }
        out
    }
}

/// A value on its way into one documented key: the TOML value a file carried,
/// or the single unquoted argument `plak config set` takes.
///
/// The two spellings differ for exactly three keys — `[root] agent` reads an
/// empty argument as *clear the pointer*, `[roster] working_set` also accepts
/// a shell shorthand, and a command-line argument is never quoted — so the
/// distinction is the value's own, not a flag beside it.
#[derive(Clone, Copy, Debug)]
enum FieldValue<'a> {
    /// A value read out of `config.toml`, with its TOML type.
    Toml(&'a toml::Value),
    /// One `plak config set` argument, as the shell handed it over.
    Cli(&'a str),
}

/// The one array `config.toml` carries, and the one sentence that names it.
const WORKING_SET_MUST_BE_AN_ARRAY: &str =
    "[roster] working_set must be an array like [\"a\", \"b\"]";

impl<'a> FieldValue<'a> {
    /// The value as a string. A file must spell it as a TOML string.
    fn string(self, key: &ConfigKey) -> Result<&'a str, String> {
        match self {
            Self::Cli(value) => Ok(value),
            Self::Toml(toml::Value::String(value)) => Ok(value.as_str()),
            Self::Toml(_) => Err(format!(
                "[{}] {} must be a string",
                key.section(),
                key.key()
            )),
        }
    }

    /// A real TOML boolean. `1`, `yes`, `0` and `no` are not booleans and the
    /// file grammar no longer pretends they are.
    fn boolean(self) -> Result<bool, String> {
        match self {
            Self::Cli("true") => Ok(true),
            Self::Cli("false") => Ok(false),
            Self::Toml(toml::Value::Boolean(value)) => Ok(*value),
            _ => Err("boolean config values must be true or false".to_owned()),
        }
    }

    /// The integer the value carries, or `None` when it is not one. The key's
    /// own bound then says what a number out of range is called.
    fn integer(self) -> Option<i64> {
        match self {
            Self::Cli(value) => value.trim().parse().ok(),
            Self::Toml(toml::Value::Integer(value)) => Some(*value),
            Self::Toml(_) => None,
        }
    }

    /// The working set: the file's own array of strings, or, from the command
    /// line, that same array or the comma- or space-separated shorthand a
    /// shell is easier to type. One key, one meaning, so a copy-paste out of
    /// `config.toml` cannot be the wrong grammar.
    fn names(self) -> Result<Vec<String>, String> {
        let array = |values: &[toml::Value]| {
            values
                .iter()
                .map(|value| match value {
                    toml::Value::String(name) => Ok(name.clone()),
                    _ => Err(WORKING_SET_MUST_BE_AN_ARRAY.to_owned()),
                })
                .collect()
        };
        match self {
            Self::Toml(toml::Value::Array(values)) => array(values),
            Self::Toml(_) => Err(WORKING_SET_MUST_BE_AN_ARRAY.to_owned()),
            Self::Cli(value) if value.trim().starts_with('[') => {
                let parsed: toml::Value = toml::from_str(&format!("v = {}", value.trim()))
                    .map(|table: toml::Table| table["v"].clone())
                    .map_err(|_| WORKING_SET_MUST_BE_AN_ARRAY.to_owned())?;
                match parsed {
                    toml::Value::Array(values) => array(&values),
                    _ => Err(WORKING_SET_MUST_BE_AN_ARRAY.to_owned()),
                }
            }
            Self::Cli(value) => Ok(value
                .split([',', ' '])
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_owned)
                .collect()),
        }
    }
}

/// A key the table does not carry. Removed keys get no named error: they are
/// unknown keys like any other.
fn unknown_key(section: &str, key: &str) -> String {
    match (section, key) {
        ("roster", _) => {
            format!("[roster] only accepts working_set; move {key} into agents/<name>.toml")
        }
        // A key above the first header belongs to no section; the whole of
        // its spelling is the name the reader has to fix.
        ("", _) => format!("unknown config key: {key}"),
        _ => format!("unknown config key: {section}.{key}"),
    }
}

#[cfg(test)]
#[test]
fn removed_keys_are_plain_unknown_keys() {
    for (body, key) in [
        (
            "[orchestration]\nwake_on_child_result = true\n",
            "orchestration.wake_on_child_result",
        ),
        (
            "[orchestration]\nchild_may_spawn = true\n",
            "orchestration.child_may_spawn",
        ),
        (
            "[orchestration]\noccupancy = 1\n",
            "orchestration.occupancy",
        ),
        (
            "[orchestration]\nmax_agents_per_cell = 1\n",
            "orchestration.max_agents_per_cell",
        ),
        ("[tui]\nbanner = true\n", "tui.banner"),
        ("[tui]\nstartup_animation = true\n", "tui.startup_animation"),
        (
            "[tui]\ncollapsed_edit_blocks = true\n",
            "tui.collapsed_edit_blocks",
        ),
        ("[tui]\nreading_width = 96\n", "tui.reading_width"),
        ("[tui]\nshow_timestamps = true\n", "tui.show_timestamps"),
    ] {
        assert_eq!(
            toml_file::parse_config(body).unwrap_err(),
            format!("unknown config key: {key} (line 2, column 1)"),
            "{body}"
        );
        assert_eq!(
            ProductConfig::default_product().set_key(key, "true"),
            Err(format!("unknown config key: {key}")),
            "{key}"
        );
    }
    for key in [
        "banner",
        "startup_animation",
        "collapsed_edit_blocks",
        "occupancy",
    ] {
        assert_eq!(
            ProductConfig::default_product().set_key(key, "true"),
            Err(format!("unknown config key: {key}")),
            "{key}"
        );
    }
    for (line, key) in [
        ("extra = \"priority\"", "extra"),
        ("max_tier = \"mutate\"", "max_tier"),
        ("name = \"t\"", "name"),
        ("may_spawn = true", "may_spawn"),
    ] {
        assert_eq!(
            toml_file::parse_template(
                &format!("account = \"codex\"\nmodel = \"m\"\n{line}\n"),
                "t"
            )
            .unwrap_err(),
            format!("template t: unknown key {key}"),
            "{line}"
        );
    }
}

/// The removed sections name their replacement.
fn removed_or_unknown_section(section: &str) -> String {
    if let Some(template) = section.strip_prefix("agent.") {
        return format!(
            "[agent.{template}] in config.toml is no longer a template; move it to ~/.plak/agents/{template}.toml"
        );
    }
    if section == "default" {
        return "unknown config section [default]; tuple fields moved to template files under ~/.plak/agents with [root] agent, and subagents moved to [orchestration]".to_owned();
    }
    if section.starts_with("roster.") {
        return format!(
            "unknown config section [{section}]; [roster] holds only working_set = [\"<template>\", ...]"
        );
    }
    if section == "tools" || section.starts_with("tools.") {
        return format!(
            "unknown config section [{section}]; [tools.<name>] takes fetch_url, web_search or x_search"
        );
    }
    format!("unknown config section [{section}]")
}

/// The seven sections `config.toml` accepts, in render order, each with the one
/// sentence the reference file prints above it.
pub const CONFIG_SECTION_DOCS: &[(&str, &str)] = &[
    ("root", "The template new sessions bind at genesis."),
    ("roster", "The templates spawnable this epoch."),
    ("orchestration", "Tree shape and spawn authority."),
    (
        "turn",
        "How long one root turn may run; there is no wall clock.",
    ),
    (
        "jobs",
        "Session-owned process budgets and hard runtime caps.",
    ),
    (
        "api",
        "Direct API-key routes: the input window new sessions are admitted under.",
    ),
    (
        "tui",
        "Terminal-app preferences; nothing here reaches a provider.",
    ),
    ("compaction", "Automatic transcript compaction."),
    (
        "telemetry",
        "Local analytics store bounds; nothing leaves the machine.",
    ),
];

/// One documented `config.toml` key.
///
/// This table is the one source for the key list: the file parser
/// ([`ProductConfig::parse`]), the `plak config set` grammar
/// ([`ProductConfig::set_key`]), the renderer behind `plak config show`,
/// `configs/reference/config.toml` ([`ProductConfig::render_annotated`]) and
/// the `docs/spec/07-config-reference.md` §3 table all read it, so none of
/// them can disagree. The default column is not stored: it is what the schema
/// default renders ([`ConfigKey::default_value`]).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConfigKey {
    /// Dotted spelling, which is also the `plak config set` key.
    pub path: &'static str,
    /// The `ProductConfig` field this key reads and writes.
    pub field: ConfigField,
    /// Value type, in the words the error messages use.
    pub kind: &'static str,
    /// Accepted range or value set; empty when the type is the whole rule.
    pub constraint: &'static str,
    /// What the key does, in one sentence.
    pub doc: &'static str,
    /// Value shown in the reference file when the key carries no value.
    pub example: &'static str,
    /// Whether a save of this key has to move through a successor epoch.
    pub scope: ConfigScope,
}

/// Whether changing a key takes a successor-epoch apply.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConfigScope {
    /// `[root]`, `[roster]` and `[orchestration]`: the tree's own rules, which
    /// only a new epoch may change.
    Epoch,
    /// Everything else: in force as soon as the file is saved.
    Immediate,
}

/// The `ProductConfig` field one key reads and writes.
///
/// Naming the field rather than matching the key spelling is what keeps the
/// parser, the setter and the renderer honest: each is one exhaustive match
/// the compiler checks, and the spellings live only in [`CONFIG_KEYS`].
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ConfigField {
    /// `[root] agent`.
    RootAgent,
    /// `[roster] working_set`.
    WorkingSet,
    /// `[orchestration] subagents`.
    Subagents,
    /// `[turn] auto_continue`.
    AutoContinue,
    /// `[orchestration] max_depth`.
    MaxDepth,
    /// `[orchestration] max_children`.
    MaxChildren,
    /// `[orchestration] max_nodes`.
    MaxNodes,
    /// Dormant checkout reclamation control.
    WorkspaceBudgetMib,
    ChildRetentionDays,
    ChildRetentionCount,
    /// Dormant checkout reclamation control.
    WorkspaceIdleSeconds,
    /// `[orchestration] max_attempts`.
    MaxAttempts,
    /// `[orchestration] transient_resumes`.
    TransientResumes,
    /// `[orchestration] max_concurrent_requests_per_account`.
    RequestsPerAccount,
    /// `[orchestration] require_review_evidence`.
    RequireReviewEvidence,
    /// `[turn] max_model_rounds`.
    MaxModelRounds,
    /// `[jobs] foreground_seconds`.
    JobsForegroundSeconds,
    /// `[jobs] max_runtime_hours`.
    JobsMaxRuntimeHours,
    /// `[api] full_window`.
    ApiFullWindow,
    /// `[tui] mouse`.
    TuiMouse,
    /// `[tui] theme`.
    Theme,
    /// `[tui] scroll_lines`.
    ScrollLines,
    /// `[tui] show_thinking`.
    ShowThinking,
    /// `[tui] group_tool_calls`.
    GroupToolCalls,
    /// `[tui] reduced_motion`.
    ReducedMotion,
    /// `[compaction] enabled`.
    CompactionEnabled,
    /// `[telemetry] max_bytes`.
    TelemetryMaxBytes,
    /// `[telemetry] max_age_days`.
    TelemetryMaxAgeDays,
}

impl ConfigField {
    /// Orchestration authority comes from the operator's product home.
    pub(crate) const fn is_home_only(self) -> bool {
        matches!(
            self,
            Self::Subagents
                | Self::AutoContinue
                | Self::MaxDepth
                | Self::MaxChildren
                | Self::MaxNodes
                | Self::MaxAttempts
                | Self::TransientResumes
                | Self::RequestsPerAccount
                | Self::RequireReviewEvidence
        )
    }
}

impl ConfigKey {
    /// Section the key lives in, without brackets.
    #[must_use]
    pub fn section(&self) -> &'static str {
        self.split().0
    }

    /// Key spelling inside the section.
    #[must_use]
    pub fn key(&self) -> &'static str {
        self.split().1
    }

    fn split(&self) -> (&'static str, &'static str) {
        match self.path.split_once('.') {
            Some(parts) => parts,
            None => (self.path, self.path),
        }
    }

    /// The literal the schema default renders, or `(unset)` for the one
    /// optional key. Derived, so a changed default cannot leave a stale
    /// comment behind in the reference file or the spec.
    #[must_use]
    pub fn default_value(&self) -> String {
        ProductConfig::default_product()
            .field_value(self.field)
            .map_or_else(|| "(unset)".to_owned(), |value| value.to_string())
    }

    /// The two comment lines the reference file prints above the key.
    #[must_use]
    pub fn comment_block(&self) -> String {
        let mut facts = format!("type: {} · default: {}", self.kind, self.default_value());
        if !self.constraint.is_empty() {
            facts.push_str(&format!(" · {}", self.constraint));
        }
        format!("\n# {}\n# {facts}\n", self.doc)
    }
}

/// The table row a `plak config set` key names by its dotted spelling.
#[must_use]
pub fn config_key(key: &str) -> Option<&'static ConfigKey> {
    CONFIG_KEYS.iter().find(|row| row.path == key)
}

/// Every `config.toml` key, in render order.
pub const CONFIG_KEYS: &[ConfigKey] = &[
    ConfigKey {
        path: "root.agent",
        field: ConfigField::RootAgent,
        kind: "string",
        constraint: "names an agents/<name>.toml file; [a-z][a-z0-9-]{0,31}, and never a section name",
        doc: "Template a new session binds at genesis; unset leaves the tuple to the CLI flags or the shell binding.",
        example: "\"root-agent\"",
        scope: ConfigScope::Epoch,
    },
    ConfigKey {
        path: "roster.working_set",
        field: ConfigField::WorkingSet,
        kind: "array of string",
        constraint: "0..=5 names, no duplicates, each a template file with a description",
        doc: "Templates the root may spawn this epoch; every other node spawns from its own template's spawns list.",
        example: "[\"worker\", \"reviewer\"]",
        scope: ConfigScope::Epoch,
    },
    ConfigKey {
        path: "orchestration.subagents",
        field: ConfigField::Subagents,
        kind: "bool",
        constraint: "true or false",
        doc: "Whether the topology tools are compiled into the root catalog at all.",
        example: "true",
        scope: ConfigScope::Epoch,
    },
    ConfigKey {
        path: "orchestration.max_depth",
        field: ConfigField::MaxDepth,
        kind: "integer",
        constraint: "1..=4",
        doc: "Deepest node the tree admits, counting the root as depth 0.",
        example: "4",
        scope: ConfigScope::Epoch,
    },
    ConfigKey {
        path: "orchestration.max_children",
        field: ConfigField::MaxChildren,
        kind: "integer",
        constraint: "1..=64",
        doc: "Active or undelivered direct children; dormant children release slots.",
        example: "8",
        scope: ConfigScope::Epoch,
    },
    ConfigKey {
        path: "orchestration.max_nodes",
        field: ConfigField::MaxNodes,
        kind: "integer",
        constraint: "1..=65",
        doc: "Root plus active, undelivered or recovery-required nodes.",
        example: "32",
        scope: ConfigScope::Epoch,
    },
    ConfigKey {
        path: "orchestration.workspace_idle_seconds",
        field: ConfigField::WorkspaceIdleSeconds,
        kind: "integer",
        constraint: "1..=65535",
        doc: "Idle seconds before lossless dormant-checkout reclamation.",
        example: "600",
        scope: ConfigScope::Epoch,
    },
    ConfigKey {
        path: "orchestration.workspace_budget_mib",
        field: ConfigField::WorkspaceBudgetMib,
        kind: "integer",
        constraint: "1..=65535",
        doc: "Soft fork-checkout disk budget in MiB; retains uncovered work.",
        example: "4096",
        scope: ConfigScope::Epoch,
    },
    ConfigKey {
        path: "orchestration.child_retention_days",
        field: ConfigField::ChildRetentionDays,
        kind: "integer",
        constraint: "1..=65535",
        doc: "Days before a sealed dormant child conversation is retired.",
        example: "7",
        scope: ConfigScope::Epoch,
    },
    ConfigKey {
        path: "orchestration.child_retention_count",
        field: ConfigField::ChildRetentionCount,
        kind: "integer",
        constraint: "1..=65535",
        doc: "Dormant conversations retained; protected work refs never expire.",
        example: "128",
        scope: ConfigScope::Epoch,
    },
    ConfigKey {
        path: "orchestration.max_attempts",
        field: ConfigField::MaxAttempts,
        kind: "integer",
        constraint: "1..=256",
        doc: "Attempts one root prompt may open across the tree.",
        example: "64",
        scope: ConfigScope::Epoch,
    },
    ConfigKey {
        path: "orchestration.transient_resumes",
        field: ConfigField::TransientResumes,
        kind: "integer",
        constraint: "0..=2",
        doc: "New continuation turns after transient failure per root user prompt or child attempt; 0 disables automatic continuation.",
        example: "1",
        scope: ConfigScope::Epoch,
    },
    ConfigKey {
        path: "orchestration.max_concurrent_requests_per_account",
        field: ConfigField::RequestsPerAccount,
        kind: "integer",
        constraint: "1..=64",
        doc: "Model requests one account may have in flight across the root and every descendant; excess requests wait in order for a slot. Takes effect on the next launch.",
        example: "8",
        scope: ConfigScope::Immediate,
    },
    ConfigKey {
        path: "orchestration.require_review_evidence",
        field: ConfigField::RequireReviewEvidence,
        kind: "bool",
        constraint: "true or false",
        doc: "When true, adopt_candidate refuses unless its evidence names a subtree attempt that referenced the candidate.",
        example: "false",
        scope: ConfigScope::Epoch,
    },
    ConfigKey {
        path: "turn.auto_continue",
        field: ConfigField::AutoContinue,
        kind: "bool",
        constraint: "true or false",
        doc: "Wake an idle interactive root when background processes finish or child results or questions arrive. Takes effect immediately; JSONL and children do not auto-continue.",
        example: "true",
        scope: ConfigScope::Immediate,
    },
    ConfigKey {
        path: "turn.max_model_rounds",
        field: ConfigField::MaxModelRounds,
        kind: "integer",
        constraint: "1..=1024",
        doc: "Model rounds one root turn may run; the next round must answer without tools. A turn has no wall clock, so this is what stops a runaway turn. Takes effect when a session next attaches; a running turn keeps the cap it was admitted under.",
        example: "512",
        scope: ConfigScope::Immediate,
    },
    ConfigKey {
        path: "jobs.foreground_seconds",
        field: ConfigField::JobsForegroundSeconds,
        kind: "integer",
        constraint: "1..=600",
        example: "30",
        doc: "Default process foreground budget before promotion. Bound at session construction and rebind; exact reopen keeps the stored value.",
        scope: ConfigScope::Immediate,
    },
    ConfigKey {
        path: "jobs.max_runtime_hours",
        field: ConfigField::JobsMaxRuntimeHours,
        kind: "integer",
        constraint: "1..=24",
        example: "10",
        doc: "Hard runtime cap per background process. Bound at session construction and rebind; exact reopen keeps the stored value.",
        scope: ConfigScope::Immediate,
    },
    ConfigKey {
        path: "api.full_window",
        field: ConfigField::ApiFullWindow,
        kind: "bool",
        constraint: "true or false",
        example: "false",
        doc: "Admit direct API sessions (openai, anthropic, xai-api) under the model's full input window instead of the provider's price threshold. Bound at session construction and rebind; exact reopen keeps the stored value.",
        scope: ConfigScope::Immediate,
    },
    ConfigKey {
        path: "tui.mouse",
        field: ConfigField::TuiMouse,
        kind: "bool",
        constraint: "",
        doc: "Mouse capture; turning it off hands text selection back to the terminal. Takes effect on the next launch.",
        example: "true",
        scope: ConfigScope::Immediate,
    },
    ConfigKey {
        path: "tui.theme",
        field: ConfigField::Theme,
        kind: "enum",
        constraint: "default-dark|default-light|catppuccin-latte|catppuccin-frappe|catppuccin-macchiato|catppuccin-mocha; dark and light are accepted aliases that normalize on the next save",
        doc: "Color palette for the whole product shell.",
        example: "\"default-dark\"",
        scope: ConfigScope::Immediate,
    },
    ConfigKey {
        path: "tui.scroll_lines",
        field: ConfigField::ScrollLines,
        kind: "integer",
        constraint: "1..=10",
        doc: "Transcript lines moved per wheel tick.",
        example: "3",
        scope: ConfigScope::Immediate,
    },
    ConfigKey {
        path: "tui.show_thinking",
        field: ConfigField::ShowThinking,
        kind: "bool",
        constraint: "subject to the provider's reasoning policy",
        doc: "Render reasoning blocks where the provider admits them.",
        example: "true",
        scope: ConfigScope::Immediate,
    },
    ConfigKey {
        path: "tui.group_tool_calls",
        field: ConfigField::GroupToolCalls,
        kind: "bool",
        constraint: "",
        doc: "Collapse consecutive tool activities to a summary when assistant prose resumes; false keeps action lines visible.",
        example: "true",
        scope: ConfigScope::Immediate,
    },
    ConfigKey {
        path: "tui.reduced_motion",
        field: ConfigField::ReducedMotion,
        kind: "bool",
        constraint: "",
        doc: "Suppress the spinner, the pulse and camera easing.",
        example: "false",
        scope: ConfigScope::Immediate,
    },
    ConfigKey {
        path: "compaction.enabled",
        field: ConfigField::CompactionEnabled,
        kind: "bool",
        constraint: "",
        doc: "Automatic transcript compaction when a session approaches its context bound.",
        example: "true",
        scope: ConfigScope::Immediate,
    },
    ConfigKey {
        path: "telemetry.max_bytes",
        field: ConfigField::TelemetryMaxBytes,
        kind: "integer",
        constraint: "greater than 0; zero is refused as ambiguous. 536870912 is 512 MiB",
        doc: "Best-effort byte retention target for the main database, WAL and SHM; busy readers or unfinished sessions may keep it above target.",
        example: "536870912",
        scope: ConfigScope::Immediate,
    },
    ConfigKey {
        path: "telemetry.max_age_days",
        field: ConfigField::TelemetryMaxAgeDays,
        kind: "integer",
        constraint: "greater than 0; zero is refused as ambiguous",
        doc: "Retention cutoff for local analytics rows, in days.",
        example: "90",
        scope: ConfigScope::Immediate,
    },
];

/// One documented `agents/<name>.toml` field.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TemplateFieldDoc {
    /// Field spelling.
    pub key: &'static str,
    /// Value type, in the words the error messages use.
    pub value_type: &'static str,
    /// Literal default, `(required)`, or `(unset)`.
    pub default: &'static str,
    /// Accepted range or value set; empty when the type is the whole rule.
    pub constraint: &'static str,
    /// What the field does, in one sentence.
    pub meaning: &'static str,
    /// Value shown in the reference file when the template leaves it unset.
    pub example: &'static str,
}

impl TemplateFieldDoc {
    /// The two comment lines the reference template prints above the field.
    #[must_use]
    pub fn comment_block(&self) -> String {
        let mut facts = format!("type: {} · default: {}", self.value_type, self.default);
        if !self.constraint.is_empty() {
            facts.push_str(&format!(" · {}", self.constraint));
        }
        format!("\n# {}\n# {facts}\n", self.meaning)
    }
}

/// Every `agents/<name>.toml` field, in render order. The file stem is the
/// template's name; the body carries no `name` key.
pub const TEMPLATE_FIELD_DOCS: &[TemplateFieldDoc] = &[
    TemplateFieldDoc {
        key: "account",
        value_type: "string",
        default: "(required)",
        constraint: "codex|chatgpt|grok|supergrok|xai-oauth|claude|openrouter|openai|anthropic|xai-api",
        meaning: "Credential lane the copy runs on.",
        example: "\"codex\"",
    },
    TemplateFieldDoc {
        key: "model",
        value_type: "string",
        default: "(required)",
        constraint: "non-empty; a direct API account must name an enrolled model, checked at load; every other account is checked at bind",
        meaning: "Exact model id.",
        example: "\"gpt-5.6-luna\"",
    },
    TemplateFieldDoc {
        key: "effort",
        value_type: "string",
        default: "(unset)",
        constraint: "none|low|medium|high|xhigh|max|ultra; Codex offers low through max (never ultra); none is an explicit API effort, not an omitted one; a direct API account requires a level on the model's published ladder, checked at load; every other account is checked at bind",
        meaning: "Reasoning effort; on a subscription account a typo surfaces as a bind failure, not a load error.",
        example: "\"medium\"",
    },
    TemplateFieldDoc {
        key: "fast_mode",
        value_type: "bool",
        default: "false",
        constraint: "true is only valid with account = \"codex\"",
        meaning: "Fast mode: request the provider's faster processing lane for this Codex subscription template. It is not a model choice and does not grant additional tools. A child runs on its own template's setting, never its parent's.",
        example: "true",
    },
    TemplateFieldDoc {
        key: "composition",
        value_type: "enum",
        default: "\"plakous-default\"",
        constraint: "direct|plakous-default|native-ptc|js-ptc|py-ptc; native-ptc is refused for openrouter, anthropic and xai-api, and on openai is valid only for a model enrolled as CodeModeOnly",
        meaning: "How the model composes the product tools: direct calls one at a time, or a programmatic tool calling (PTC) cell. native-ptc is the provider's own PTC system (today only Codex Code Mode is enrolled); js-ptc and py-ptc are harness PTC on every account. plakous-default uses the versioned Plakous-recommended tool presentation for this provider/model; the resolved presentation is pinned for the session and does not select or substitute a model.",
        example: "\"plakous-default\"",
    },
    TemplateFieldDoc {
        key: "protocol",
        value_type: "enum",
        default: "\"auto\"",
        constraint: "auto|chat|responses|messages; anything but auto is OpenRouter-only",
        meaning: "Wire protocol pin for an OpenRouter model whose family the gateway reports wrongly.",
        example: "\"auto\"",
    },
    TemplateFieldDoc {
        key: "description",
        value_type: "string",
        default: "\"\"",
        constraint: "required and non-blank for a template named in [roster] working_set or in a spawns list",
        meaning: "The spawn card a parent routes on: what this agent is for, in the parent's words.",
        example: "\"implements a scoped change and reports the diff\"",
    },
    TemplateFieldDoc {
        key: "access",
        value_type: "enum",
        default: "\"full\"",
        constraint: "full|read-execute",
        meaning: "Edit ceiling: full includes file-edit tools; read-execute omits dedicated file-edit tools but permits shell commands. Neither is an OS sandbox. A spawn request above the ceiling is clipped, not refused.",
        example: "\"read-execute\"",
    },
    TemplateFieldDoc {
        key: "tools",
        value_type: "array of string",
        default: "[]",
        constraint: "web_search|web_fetch|x_search|fetch_url|image_gen|mcp:<id>|mcp:<id>/<tool>, none twice; each mcp:<id> names a [mcp_servers.<id>] table; a child gets exactly its own list, never its parent's",
        meaning: "Optional tools enabled by name after the core tools; options come from config.toml [tools.<name>] and [mcp_servers.<id>]. Each is resolved against the route when a session or child binds, and a tool the route cannot serve refuses the bind.",
        example: "[\"web_search\", \"mcp:github\"]",
    },
    TemplateFieldDoc {
        key: "spawns",
        value_type: "array of string",
        default: "[]",
        constraint: "0..=5 names, no duplicates, each a template file with a description",
        meaning: "The templates a copy of this template may spawn; its own name is never on its menu. Empty is a leaf with no spawn_agent. The root spawns [roster] working_set instead; this list applies where the template runs as a child.",
        example: "[\"worker\", \"reviewer\"]",
    },
    TemplateFieldDoc {
        key: "may_adopt",
        value_type: "bool",
        default: "false",
        constraint: "requires spawns naming another template",
        meaning: "Whether a copy may adopt a sealed candidate out of its own subtree.",
        example: "false",
    },
    TemplateFieldDoc {
        key: "delegation",
        value_type: "enum",
        default: "\"explicit\"",
        constraint: "explicit|proactive",
        meaning: "Whether the model is told to delegate unprompted or only when asked.",
        example: "\"explicit\"",
    },
    TemplateFieldDoc {
        key: "cost_tier",
        value_type: "integer",
        default: "(unset)",
        constraint: "0..=255",
        meaning: "Static label a parent sees on the spawn card; never a measured cost.",
        example: "2",
    },
    TemplateFieldDoc {
        key: "max_turns",
        value_type: "integer",
        default: "(unset)",
        constraint: "1 or more",
        meaning: "Turn budget for a copy of this template.",
        example: "24",
    },
    TemplateFieldDoc {
        key: "on_mint",
        value_type: "string",
        default: "(unset)",
        constraint: "not validated at parse; runs in the copy's workspace",
        meaning: "Command run once when a copy is minted, before its first turn.",
        example: "\"cargo fetch\"",
    },
    TemplateFieldDoc {
        key: "show_model",
        value_type: "bool",
        default: "false",
        constraint: "",
        meaning: "Put the model id on the spawn card next to the description.",
        example: "false",
    },
    TemplateFieldDoc {
        key: "env",
        value_type: "table of string",
        default: "{}",
        constraint: "keys are not validated; values are strings",
        meaning: "Environment for on_mint only; not passed to the copy's tool calls.",
        example: "GIT_TERMINAL_PROMPT = \"0\"",
    },
];

/// One template file body with the reference commentary attached.
///
/// Fields the template leaves unset print as a commented example, so every
/// field of the schema is visible in `configs/reference/agents/*.toml`.
#[must_use]
pub fn render_template_annotated(name: &str, template: &AgentTemplate) -> String {
    let rendered = render_template(template);
    let mut out = format!(
        "# Plak agent template (non-secret): ~/.plak/agents/{name}.toml\n\
         #\n\
         # One flat object plus an optional [env] table. The file stem is the\n\
         # template's name. A project's .plak/agents/<name>.toml wins over the\n\
         # user's file of the same name. An unknown key is a named load error.\n"
    );
    for doc in TEMPLATE_FIELD_DOCS {
        out.push_str(&doc.comment_block());
        if doc.key == "env" {
            let table = rendered
                .split_once("\n[env]\n")
                .map(|(_, table)| table.to_owned());
            match table {
                Some(table) => {
                    out.push_str("[env]\n");
                    out.push_str(&table);
                }
                None => out.push_str(&format!("# [env]\n# {}\n", doc.example)),
            }
            continue;
        }
        match rendered
            .lines()
            .find(|line| line.starts_with(&format!("{} = ", doc.key)))
        {
            Some(line) => out.push_str(&format!("{line}\n")),
            None => out.push_str(&format!("# {} = {}\n", doc.key, doc.example)),
        }
    }
    out
}

/// One template file body, canonical field order. Also the form the epoch
/// digest covers.
#[must_use]
pub(crate) fn render_template(template: &AgentTemplate) -> String {
    fn text(out: &mut String, key: &str, value: &str) {
        out.push_str(&format!("{key} = {}\n", toml_text(value)));
    }
    let mut out = String::from("# Plak agent template (non-secret)\n");
    text(&mut out, "account", &template.account);
    text(&mut out, "model", &template.model);
    if let Some(effort) = &template.effort {
        text(&mut out, "effort", effort);
    }
    out.push_str(&format!(
        "fast_mode = {}\n",
        template.extra.as_deref() == Some("priority")
    ));
    if let Some(composition) = template.composition {
        text(&mut out, "composition", composition.as_str());
    }
    if let Some(protocol) = template.protocol {
        text(&mut out, "protocol", protocol.as_str());
    }
    text(&mut out, "description", &template.description);
    text(
        &mut out,
        "access",
        match template.max_tier {
            Tier::Mutate => "full",
            Tier::ReadOnly | Tier::Execute => "read-execute",
        },
    );
    if !template.tools.is_empty() {
        let tools: Vec<String> = template
            .tools
            .entries()
            .iter()
            .map(|tool| toml_text(&tool.to_string()))
            .collect();
        out.push_str(&format!("tools = [{}]\n", tools.join(", ")));
    }
    let spawns: Vec<String> = template
        .spawns
        .iter()
        .map(|name| toml_text(name.as_str()))
        .collect();
    out.push_str(&format!("spawns = [{}]\n", spawns.join(", ")));
    out.push_str(&format!("may_adopt = {}\n", template.may_adopt));
    out.push_str(&format!(
        "delegation = {}\n",
        toml_text(template.delegation.as_str())
    ));
    if let Some(cost_tier) = template.cost_tier {
        out.push_str(&format!("cost_tier = {cost_tier}\n"));
    }
    if let Some(max_turns) = template.max_turns {
        out.push_str(&format!("max_turns = {max_turns}\n"));
    }
    if let Some(on_mint) = &template.on_mint {
        out.push_str(&format!("on_mint = {}\n", toml_text(on_mint)));
    }
    out.push_str(&format!("show_model = {}\n", template.show_model));
    if !template.env.is_empty() {
        out.push_str("\n[env]\n");
        for (key, value) in &template.env {
            out.push_str(&format!("{} = {}\n", toml_key(key), toml_text(value)));
        }
    }
    out
}

/// One string as TOML spells it, quotes and escapes included. A description
/// carrying a quote, a backslash or a newline renders as a file that reads
/// back as the same string.
fn toml_text(value: &str) -> String {
    Value::from(value).to_string()
}

/// One `[env]` key: bare where TOML admits a bare key, quoted otherwise.
fn toml_key(key: &str) -> String {
    let bare = !key.is_empty()
        && key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-');
    if bare { key.to_owned() } else { toml_text(key) }
}

/// Parses one flat template file. `name` is the file stem; the body carries
/// no `name` key.
pub(crate) fn parse_template(text: &str, name: &str) -> Result<AgentTemplate, String> {
    toml_file::parse_template(text, name)
}

/// The templates and per-name origins one agents directory produced.
type TaggedTemplates = (
    BTreeMap<String, AgentTemplate>,
    BTreeMap<String, ConfigValueOrigin>,
);

/// Every `*.toml` under one agents directory, each name tagged with the file
/// it came from. The caller decides which directories participate and in
/// which order — the loader itself knows no precedence.
fn load_templates_dir(dir: &Path, origin: ConfigValueOrigin) -> Result<TaggedTemplates, String> {
    let mut templates = BTreeMap::new();
    let mut origins = BTreeMap::new();
    for (name, path) in template_files(dir)? {
        let text = fs::read_to_string(&path)
            .map_err(|_| format!("could not read template {}", path.display()))?;
        let template =
            parse_template(&text, &name).map_err(|error| format!("{}: {error}", path.display()))?;
        templates.insert(name.clone(), template);
        origins.insert(name, origin);
    }
    Ok((templates, origins))
}

fn template_files(dir: &Path) -> Result<Vec<(String, PathBuf)>, String> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(_) => return Err(format!("could not read {}", dir.display())),
    };
    let mut files = Vec::new();
    for entry in entries {
        let path = entry
            .map_err(|_| format!("could not read {}", dir.display()))?
            .path();
        if path.extension().is_none_or(|extension| extension != "toml") || !path.is_file() {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            return Err(format!(
                "template file {} has no UTF-8 name",
                path.display()
            ));
        };
        if stem.starts_with('.') {
            continue;
        }
        validate_template_name(stem).map_err(|error| format!("{}: {error}", path.display()))?;
        files.push((stem.to_owned(), path));
    }
    files.sort();
    Ok(files)
}

fn parse_bound(key: &str, value: FieldValue<'_>, ceiling: u16) -> Result<u16, String> {
    match value.integer().and_then(|bound| u16::try_from(bound).ok()) {
        Some(bound) if (1..=ceiling).contains(&bound) => Ok(bound),
        _ => Err(format!(
            "[orchestration] {key} must be an integer between 1 and {ceiling}"
        )),
    }
}

fn parse_max_model_rounds(value: FieldValue<'_>) -> Result<u16, String> {
    match value
        .integer()
        .and_then(|rounds| u16::try_from(rounds).ok())
    {
        Some(rounds) if (1..=CEILING_MAX_MODEL_ROUNDS).contains(&rounds) => Ok(rounds),
        _ => Err(format!(
            "[turn] max_model_rounds must be an integer between 1 and {CEILING_MAX_MODEL_ROUNDS}"
        )),
    }
}

fn parse_scroll_lines(value: FieldValue<'_>) -> Result<u16, String> {
    match value.integer().and_then(|lines| u16::try_from(lines).ok()) {
        Some(lines) if (MIN_SCROLL_LINES..=MAX_SCROLL_LINES).contains(&lines) => Ok(lines),
        _ => Err(format!(
            "[tui] scroll_lines must be an integer between {MIN_SCROLL_LINES} and {MAX_SCROLL_LINES}"
        )),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AtomicStage {
    Write,
    FileSync,
    Publish,
    ParentSync,
}

/// One file publication primitive. New template names use an exclusive link
/// instead of replacing an existing file; the config pointer uses rename.
fn atomic_file(
    target: &Path,
    bytes: &[u8],
    create_new: bool,
    fault: &mut impl FnMut(&Path, AtomicStage) -> Result<(), String>,
) -> Result<(), ConfigSaveError> {
    let parent = target.parent().ok_or_else(|| {
        ConfigSaveError::BeforePublication("publication target has no parent".into())
    })?;
    let temporary = parent.join(format!(".publication.{}.tmp", uuid::Uuid::new_v4()));
    let before = |message: &str| ConfigSaveError::BeforePublication(message.to_owned());
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)
        .map_err(|_| before("could not create publication temporary file"))?;
    let publish = (|| -> Result<(), String> {
        fault(target, AtomicStage::Write)?;
        file.write_all(bytes)
            .map_err(|_| "could not write publication".to_owned())?;
        fault(target, AtomicStage::FileSync)?;
        file.sync_all()
            .map_err(|_| "could not sync publication".to_owned())?;
        fault(target, AtomicStage::Publish)?;
        if create_new {
            fs::hard_link(&temporary, target)
        } else {
            fs::rename(&temporary, target)
        }
        .map_err(|_| "could not publish file".to_owned())?;
        Ok(())
    })();
    let _ = fs::remove_file(&temporary);
    publish.map_err(ConfigSaveError::BeforePublication)?;
    fault(target, AtomicStage::ParentSync)
        .and_then(|()| sync_parent_directory(parent))
        .map_err(ConfigSaveError::PublishedDurabilityUncertain)
}

fn sync_parent_directory(parent: &Path) -> Result<(), String> {
    OpenOptions::new()
        .read(true)
        .open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|_| "could not sync publication directory".to_owned())
}

/// `telemetry.*` bounds: zero is ambiguous between "unlimited" and
/// "disabled", so it is refused; the pruner relies on a finite cutoff.
fn parse_positive(key: &str, value: FieldValue<'_>) -> Result<u64, String> {
    match value
        .integer()
        .and_then(|parsed| u64::try_from(parsed).ok())
    {
        Some(parsed) if parsed > 0 => Ok(parsed),
        _ => Err(format!("{key} must be a positive integer")),
    }
}

/// A `u64` the product holds for the telemetry store as the `i64` a TOML
/// integer is. Neither bound can have come from a file above the TOML
/// maximum, so the saturation is unreachable from a config and is here so the
/// renderer has no panic in it.
fn saturating_toml_integer(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

pub fn init_if_missing(paths: &PlakPaths) -> Result<bool, String> {
    paths.ensure_layout()?;
    let path = paths.config_file();
    if path.is_file() {
        return Ok(false);
    }
    let rendered = ProductConfig::default_product().render_annotated();
    let mut template = String::from(
        "# Uncomment only the preferences you want to pin. Omitted keys follow defaults.\n",
    );
    // Section headers stay live (an empty known section parses), so a key
    // uncommented per the instruction above lands in its own section.
    for line in rendered.lines() {
        if line.starts_with('#') || line.starts_with('[') || line.is_empty() {
            template.push_str(line);
        } else {
            template.push_str("# ");
            template.push_str(line);
        }
        template.push('\n');
    }
    atomic_file(&path, template.as_bytes(), true, &mut |_, _| Ok(()))
        .map_err(ConfigSaveError::into_message)?;
    Ok(true)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests {
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
            "[orchestration]\nsubagents = false\nmax_depth = 2\nmax_children = 3\nmax_nodes = 8\nmax_attempts = 12\ntransient_resumes = 0\nmax_concurrent_requests_per_account = 3\nrequire_review_evidence = true\n[turn]\nauto_continue = false\n",
        )
        .unwrap();
        let home = project_home(&project);
        fs::create_dir_all(&home).unwrap();
        fs::write(
            home.join("config.toml"),
            "[orchestration]\nsubagents = true\nmax_depth = 4\nmax_children = 8\nmax_nodes = 32\nmax_attempts = 64\ntransient_resumes = 2\nmax_concurrent_requests_per_account = 9\nrequire_review_evidence = false\n[turn]\nauto_continue = true\n[tui]\nmouse = false\n",
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
        let paths = PlakPaths::explicit(temp.path().join(".plak"))
            .with_project_root(temp.path().to_path_buf());
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
        let error = parse_template("account = \"grok\"\nmodel = \"m\"\nfast_mode = true\n", "t")
            .unwrap_err();
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
        let needs = "template t: openai model `gpt-5.6-sol` requires --effort none|low|medium|high|xhigh|max";
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
            let error =
                parse_template(&format!("account = \"codex\"\nmodel = \"m\"\n{bad}\n"), "t")
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
        let roster_error =
            ProductConfig::parse("[roster.review]\naccount = \"codex\"\n").unwrap_err();
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
        let (_tmp, paths) =
            home_with_templates(&[("quiet", "account = \"codex\"\nmodel = \"m\"\n")]);
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
        assert!(error.contains("template quiet is in template lead's spawns and needs a description for its spawn card"), "{error}");
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
        ProductConfig::load(&PlakPaths::explicit(
            repo_configs_dir().join("providers/codex"),
        ))
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
        // The reference tree is the annotated render of `configs/providers/codex`.
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
            (
                "providers/codex",
                Some("codex"),
                provider_codex,
                codex_tools,
            ),
            (
                "providers/claude",
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
                "providers/grok",
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
                "providers/multi",
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
            let dir = repo_configs_dir().join(preset);
            let paths =
                PlakPaths::resolve(Some(std::ffi::OsStr::new(&dir)), None).expect("preset home");
            let loaded = ProductConfig::load(&paths)
                .unwrap_or_else(|error| panic!("configs/{preset}: {error}"));
            assert_eq!(
                loaded.root_agent.as_deref(),
                Some("root-agent"),
                "configs/{preset} must bind root-agent"
            );
            assert_eq!(
                loaded
                    .working_set
                    .names()
                    .iter()
                    .map(TemplateName::as_str)
                    .collect::<Vec<_>>(),
                ["worker", "reviewer", "researcher"],
                "configs/{preset} working set"
            );
            assert_eq!(loaded.templates.len(), 4, "configs/{preset}");
            assert!(loaded.subagents, "configs/{preset}");
            assert_eq!(loaded.max_depth, 1, "configs/{preset}");
            assert_eq!(loaded.max_children, 8, "configs/{preset}");
            for (name, template) in &loaded.templates {
                assert_eq!(
                    template.delegation,
                    Delegation::Explicit,
                    "configs/{preset}/{name}"
                );
                assert!(
                    !template.may_adopt,
                    "configs/{preset}/{name} must not adopt"
                );
                if let Some(provider) = provider {
                    assert_eq!(
                        template.account, provider,
                        "configs/{preset}/{name} must bind the preset's own provider"
                    );
                }
            }
            // (role, access, spawns, tools)
            let shape: [(&str, Tier, &[&str], &[&str]); 4] = [
                ("root-agent", Tier::Mutate, &[], &[]),
                ("worker", Tier::Mutate, &["researcher"], &[]),
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
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../docs/spec/07-config-reference.md"),
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
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../docs/spec/07-config-reference.md"),
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
            let before = format!(
                "# hand written\n\n[tui]\ntheme = \"{alias}\"\nmouse    =   false   # kept\n"
            );
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
}

#[cfg(test)]
mod job_config_tests {
    use super::*;

    #[test]
    fn jobs_keys_are_closed_bounded_and_round_trip() {
        let defaults = ProductConfig::default_product();
        assert_eq!(defaults.jobs_foreground_seconds, 30);
        assert_eq!(defaults.jobs_max_runtime_hours, 10);
        let config =
            ProductConfig::parse("[jobs]\nforeground_seconds = 600\nmax_runtime_hours = 24\n")
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
}
