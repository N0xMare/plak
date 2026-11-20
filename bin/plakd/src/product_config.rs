//! Product `config.toml` (non-secret preferences).

mod atomic;
#[cfg(feature = "web")]
pub(crate) mod console;
mod forward;
mod keys;
mod load;
pub(crate) mod mcp_edit;
mod policy;
mod publication;
mod readiness;
#[cfg(test)]
#[path = "product_config/settings_tests.rs"]
pub(crate) mod settings;
mod templates;
mod toml_file;
mod tool_sections;
pub(crate) mod web;

pub(crate) use tool_sections::runners::is_runner_command;
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
    ConfigSaveError, ConfigValueOrigin, ConfigWriteScope, ProductConfig, ProjectConfigSource,
    no_native_protocol_accounts, template_accounts,
};

pub use keys::{
    CONFIG_KEYS, CONFIG_SECTION_DOCS, ConfigField, ConfigKey, ConfigScope, ResumeAfterLoss,
    config_key,
};
pub use policy::{
    AgentTemplate, CEILING_MAX_ATTEMPTS, CEILING_MAX_CHILDREN, CEILING_MAX_DEPTH,
    CEILING_MAX_MODEL_ROUNDS, CEILING_MAX_NODES, CEILING_REQUESTS_PER_ACCOUNT,
    DEFAULT_MAX_MODEL_ROUNDS, DefaultBinding, MINTED_ROOT_TEMPLATE_NAME, SessionPolicy,
    TelemetryConfig, topology_admission_probe, validate_template_name,
};

pub use readiness::{
    CredentialReadiness, TemplateNotReady, TemplateReadiness, TemplateReadinessRow,
    catalog_credential_readiness, template_readiness,
};

pub use templates::{
    TEMPLATE_FIELD_DOCS, TemplateFieldDoc, init_if_missing, render_template_annotated,
};

pub(crate) use templates::{parse_template, render_template};

pub(crate) fn install_gateway_address(
    paths: &crate::home::paths::PlakPaths,
    address: &str,
) -> Result<(), String> {
    let _lock = publication::ConfigLock::take(paths).map_err(ConfigSaveError::into_message)?;
    let path = paths.config_file();
    let (text, missing) = match std::fs::read_to_string(&path) {
        Ok(text) => (text, false),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (String::new(), true),
        Err(error) => return Err(format!("could not read {}: {error}", path.display())),
    };
    let mut config = toml_file::parse_config(&text)?;
    if let Some(existing) = config.gateway_address.as_deref() {
        return if existing == address {
            Ok(())
        } else {
            Err(format!(
                "host install will not replace gateway.address in {}; remove the existing address to install the local gateway",
                path.display(),
            ))
        };
    }
    config.gateway_address = Some(address.to_owned());
    let edited = toml_file::edit_document(Some(&text), &config)?;
    atomic::atomic_file(&path, edited.as_bytes(), missing, &mut |_, _| Ok(()))
        .map_err(ConfigSaveError::into_message)
}

#[cfg(test)]
mod tool_sections_tests;

#[cfg(test)]
mod tests;
