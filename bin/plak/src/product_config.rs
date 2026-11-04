//! Product `config.toml` (non-secret preferences).

mod atomic;
mod forward;
mod keys;
mod load;
#[cfg(feature = "mcp")]
pub(crate) mod mcp_edit;
mod policy;
mod readiness;
#[cfg(any(test, feature = "tui"))]
pub(crate) mod settings;
mod templates;
mod toml_file;
mod tool_sections;
pub(crate) mod web;

pub use tool_sections::runners::{LOCAL_RUNNER_NAME, RunnerConfig, validate_runner_name};
pub use tool_sections::{
    DEFAULT_FETCH_URL_MAX_BYTES, DEFAULT_FETCH_URL_TIMEOUT_SECONDS, DEFAULT_MCP_CALL_SECONDS,
    DEFAULT_MCP_STARTUP_SECONDS, DEFAULT_WEB_SEARCH_MAX_USES, FetchUrlOptions, McpOAuthClient,
    McpOAuthConfig, McpServerConfig, McpTransport, ToolSections, WebSearchOptions, XSearchOptions,
};

/// Per-repository trust for `.plak/config.toml` and `.plak/agents/`.
pub mod project_trust;

// Session-binding vocabulary lives in `plakous_hosted::binding`; these names stay for
// the readers that spell them the configuration way.
pub use plakous_hosted::binding::RootTemplateFact;
pub use plakous_hosted::binding::{
    CompositionSelection as ProductCompositionSelection,
    OpenRouterRoutingSelection as ProductOpenRouterRoutingSelection,
    ProtocolSelection as ProductProtocolSelection, ResolvedProtocol as ProductResolvedProtocol,
    TargetSelection as ProductTargetSelection,
    WIRE_RESOLUTION_REVISION as PRODUCT_WIRE_RESOLUTION_REVISION,
    WireBinding as ProductWireBinding,
};

pub use load::{
    ConfigSaveError, ConfigValueOrigin, ConfigWriteScope, ProductConfig, ProductTheme,
    ProjectConfigSource, no_native_protocol_accounts, template_accounts,
};

pub use keys::{
    CONFIG_KEYS, CONFIG_SECTION_DOCS, ConfigField, ConfigKey, ConfigScope, ResumeAfterLoss,
    config_key,
};
pub use policy::{
    AgentTemplate, CEILING_MAX_ATTEMPTS, CEILING_MAX_CHILDREN, CEILING_MAX_DEPTH,
    CEILING_MAX_MODEL_ROUNDS, CEILING_MAX_NODES, CEILING_REQUESTS_PER_ACCOUNT,
    DEFAULT_MAX_MODEL_ROUNDS, DEFAULT_SCROLL_LINES, DefaultBinding, MAX_SCROLL_LINES,
    MIN_SCROLL_LINES, MINTED_ROOT_TEMPLATE_NAME, SessionPolicy, TelemetryConfig,
    topology_admission_probe, validate_template_name,
};

pub use readiness::{
    CredentialReadiness, TemplateNotReady, TemplateReadiness, TemplateReadinessRow,
    catalog_credential_readiness, template_readiness,
};

pub use templates::{
    TEMPLATE_FIELD_DOCS, TemplateFieldDoc, init_if_missing, render_template_annotated,
};

pub(crate) use templates::{parse_template, render_template};

#[cfg(test)]
mod tool_sections_tests;

#[cfg(test)]
mod tests;
