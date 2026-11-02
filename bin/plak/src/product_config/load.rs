//! Load, merge, save, and explain product configuration.

use super::atomic::{AtomicStage, atomic_file, sync_parent_directory};
use super::keys::{
    CONFIG_KEYS, CONFIG_SECTION_DOCS, ConfigField, ConfigKey, FieldValue,
    check_client_id_metadata_url, config_key, parse_bound, parse_max_model_rounds, parse_positive,
    parse_scroll_lines, saturating_toml_integer,
};
use super::policy::{
    AgentTemplate, CEILING_MAX_ATTEMPTS, CEILING_MAX_CHILDREN, CEILING_MAX_DEPTH,
    CEILING_MAX_NODES, CEILING_REQUESTS_PER_ACCOUNT, DEFAULT_MAX_MODEL_ROUNDS,
    DEFAULT_SCROLL_LINES, DefaultBinding, MINTED_ROOT_TEMPLATE_NAME, SessionPolicy,
    TelemetryConfig, validate_template_name,
};
use super::readiness::{TemplateReadinessRow, catalog_credential_readiness, template_readiness};
use super::templates::{load_templates_dir, parse_template, render_template};
use super::{
    ProductCompositionSelection, ProductProtocolSelection, RootTemplateFact, ToolSections,
    project_trust, toml_file,
};
use crate::home::paths::PlakPaths;
use plakous_hosted::agent_tree_protocol::{
    DEFAULT_DIRECT_CHILDREN, DEFAULT_TREE_ATTEMPTS, DEFAULT_TREE_NODES, WorkingSet, WriterMode,
};
use plakous_hosted::tool_plan::SelectedTool;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use toml_edit::Value;

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
    pub(super) present: BTreeSet<ConfigField>,
    /// `[root] agent`: the template new sessions bind at genesis.
    pub root_agent: Option<String>,
    /// `[root] composition`: a root-only override, not a child template field.
    pub root_composition: Option<ProductCompositionSelection>,
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
    /// `[tui] reload_requests`: accept same-user reload requests (home-scoped).
    pub reload_requests: bool,
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

impl PartialEq for ProductConfig {
    fn eq(&self, other: &Self) -> bool {
        self.templates == other.templates
            && self.root_agent == other.root_agent
            && self.root_composition == other.root_composition
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
            && self.reload_requests == other.reload_requests
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

pub(super) fn is_template_account(account: &str) -> bool {
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

    pub(super) fn parse(value: &str) -> Result<Self, String> {
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
                self.root_composition
                    .unwrap_or_else(|| template.composition_or_default())
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

    pub(super) fn load_config_file(path: &Path) -> Result<Self, String> {
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
            ConfigField::RootComposition => self.root_composition = source.root_composition,
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
            ConfigField::ReloadRequests => self.reload_requests = source.reload_requests,
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
            root_composition: None,
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
            reload_requests: false,
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
        self.root_composition.unwrap_or_else(|| {
            self.root_template()
                .map_or(ProductCompositionSelection::FollowCatalog, |template| {
                    template.composition_or_default()
                })
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
        self.root_composition = None;
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
    /// `docs/reference-config/config.toml` is exactly this output for the schema
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
    pub(super) fn field_value(&self, field: ConfigField) -> Option<Value> {
        Some(match field {
            ConfigField::RootAgent => Value::from(self.root_agent.as_deref()?),
            ConfigField::RootComposition => Value::from(self.root_composition?.as_str()),
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
            ConfigField::ReloadRequests => Value::from(self.reload_requests),
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

    #[allow(
        clippy::too_many_lines,
        reason = "Project save preserves the stage, permission, sync, replace, and injected-failure cleanup sequence as one transaction."
    )]
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
        if let Some(template) = self.root_template() {
            template
                .validate_composition(self.root_composition.or(template.composition))
                .map_err(|error| format!("[root] composition: {error}"))?;
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
        if candidate.root_agent != self.root_agent {
            candidate.root_composition = None;
        }
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
    pub(super) fn assign_field(
        &mut self,
        key: &ConfigKey,
        value: FieldValue<'_>,
    ) -> Result<(), String> {
        match key.field {
            ConfigField::RootComposition => {
                self.root_composition = Some(ProductCompositionSelection::parse_config(
                    value.string(key)?,
                )?);
            }
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
            ConfigField::ReloadRequests => self.reload_requests = value.boolean()?,
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

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests;
