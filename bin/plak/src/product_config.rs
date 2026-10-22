//! Product `config.toml` (non-secret preferences).

use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use toml_edit::Value;

use crate::home::paths::PlakPaths;

pub(crate) mod mcp_edit;
mod toml_file;
mod tool_sections;

pub use tool_sections::{
    DEFAULT_FETCH_URL_MAX_BYTES, DEFAULT_FETCH_URL_TIMEOUT_SECONDS, DEFAULT_MCP_CALL_SECONDS,
    DEFAULT_MCP_STARTUP_SECONDS, DEFAULT_WEB_SEARCH_MAX_USES, FetchUrlOptions, McpOAuthClient,
    McpOAuthConfig, McpServerConfig, McpTransport, ToolSections, WebSearchOptions, XSearchOptions,
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
    /// `[mcp] client_id_metadata_url`: the Client ID Metadata Document an
    /// MCP login may register with; home-scoped.
    pub mcp_client_id_metadata_url: Option<String>,
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
            && self.mcp_client_id_metadata_url == other.mcp_client_id_metadata_url
            && self.tool_sections == other.tool_sections
    }
}

impl Eq for ProductConfig {}

/// Every account a `[templates.*]` card may name, and so every account a
/// tree epoch may pin. The product owns this roster; the tree reads it as
/// frozen `bind_hints` and only checks membership.
pub fn template_accounts() -> impl Iterator<Item = &'static str> {
    plakous_hosted::capabilities::account_ids()
}

/// Accounts with no Codex Code Mode, so a `native-ptc` card on them is a
/// wire-vocabulary error. The `openai` API wears the Codex pack: its cards are
/// checked against the enrolled model table at load instead.
pub fn no_native_protocol_accounts() -> impl Iterator<Item = &'static str> {
    plakous_hosted::capabilities::no_native_protocol_accounts()
}

fn is_template_account(account: &str) -> bool {
    template_accounts().any(|known| known == account)
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
            if overlay
                .present
                .iter()
                .any(|field| field.is_home_only() && *field != ConfigField::McpClientIdMetadataUrl)
            {
                self.warnings.push(format!(
                    "{}: [orchestration] is home-scoped and ignored here; set it in {}",
                    config_path.display(),
                    paths.config_file().display(),
                ));
            }
            if overlay
                .present
                .contains(&ConfigField::McpClientIdMetadataUrl)
            {
                self.warnings.push(format!(
                    "{}: [mcp] client_id_metadata_url is home-scoped and ignored here; set it in {}",
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
            ConfigField::McpClientIdMetadataUrl => {
                self.mcp_client_id_metadata_url = source.mcp_client_id_metadata_url.clone();
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
            mcp_client_id_metadata_url: None,
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
    /// unset (`[root] agent` and `[mcp] client_id_metadata_url`).
    fn value_line(&self, key: &ConfigKey) -> Option<String> {
        Some(format!(
            "{} = {}\n",
            key.key(),
            self.field_value(key.field)?
        ))
    }

    /// One key's value as `config.toml` carries it, or `None` for a key this
    /// config leaves unset (`[root] agent` and `[mcp] client_id_metadata_url`).
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
            ConfigField::McpClientIdMetadataUrl => {
                Value::from(self.mcp_client_id_metadata_url.as_deref()?)
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
            ConfigField::McpClientIdMetadataUrl => {
                // Empty clears it from the command line, as for [root] agent.
                let url = value.string(key)?;
                if matches!(value, FieldValue::Cli(_)) && url.is_empty() {
                    self.mcp_client_id_metadata_url = None;
                } else {
                    check_client_id_metadata_url(url)?;
                    self.mcp_client_id_metadata_url = Some(url.to_owned());
                }
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
    (
        "mcp",
        "The MCP client; servers are [mcp_servers.<id>] tables.",
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
    /// `[mcp] client_id_metadata_url`.
    McpClientIdMetadataUrl,
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
                | Self::McpClientIdMetadataUrl
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

    /// The literal the schema default renders, or `(unset)` for an
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
    ConfigKey {
        path: "mcp.client_id_metadata_url",
        field: ConfigField::McpClientIdMetadataUrl,
        kind: "string",
        constraint: "https:// URL with a path, no query or fragment; home-scoped",
        doc: "The OAuth Client ID Metadata Document that identifies Plak to an MCP authorization server that supports one (spec 01 §8.5). Unset disables that registration path; the document lists the redirect URI http://127.0.0.1/callback, which a server matches on any port. Takes effect at the next plak mcp login.",
        example: "\"https://plak.example/oauth/client.json\"",
        scope: ConfigScope::Immediate,
    },
];

/// A Client ID Metadata Document URL: `https://` with a path beyond `/`,
/// no query, fragment or credentials.
fn check_client_id_metadata_url(url: &str) -> Result<(), String> {
    let refused = || {
        "mcp.client_id_metadata_url must be an https:// URL with a path, without query or fragment"
            .to_owned()
    };
    let rest = url.strip_prefix("https://").ok_or_else(refused)?;
    let (authority, path) = rest.split_once('/').ok_or_else(refused)?;
    if authority.is_empty()
        || authority.contains('@')
        || path.is_empty()
        || url.contains(['?', '#'])
        || url
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
    {
        return Err(refused());
    }
    Ok(())
}

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
mod tests;
