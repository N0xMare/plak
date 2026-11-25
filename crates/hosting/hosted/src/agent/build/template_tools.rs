use crate::account::{AccountRef, Vertical};
use crate::agent_tree_protocol::Tier;
use crate::binding::CompositionSelection;
use crate::tool_plan::{NodeToolSelection, ToolRoute};
use crate::tools::{ProductToolExtensions, product_surface_with_fence};
use crate::{CliFailure, CliResult};
use plakous::model::{
    Digest, DigestDomain, EffectiveProviderConfigurationId, IntegrationClass, ModelId,
};
use plakous::tools::{
    JobConfig, PROCESS_TOOL_PROJECTION_REVISION, ProviderModelProjectionIdentityV2,
};

/// A tool in the definition-time portion of a node's callable surface.
pub struct TemplateTool {
    /// Model-visible name, including any composition-specific alternate.
    pub name: String,
    /// Whether a template extension supplied this tool.
    pub extra: bool,
}

/// Compile the definition-time menu through the runtime's tool plan, registry
/// and surface compiler. Workspace skills and topology grants are supplied
/// only at attach/spawn. MCP listings are likewise not opened by this read.
///
/// # Errors
/// Refuses tool selections or compositions that cannot be bound.
pub fn template_tools(
    account: &AccountRef,
    model: &str,
    composition: CompositionSelection,
    access: Tier,
    tools: &NodeToolSelection,
) -> CliResult<Vec<TemplateTool>> {
    let vertical = account.vertical();
    let native = vertical.capability().native_provider();
    let route = match vertical {
        Vertical::Codex => ToolRoute::Codex,
        Vertical::Grok => ToolRoute::Grok,
        Vertical::Claude => ToolRoute::Claude,
        Vertical::OpenAi => ToolRoute::OpenAi,
        Vertical::Anthropic => ToolRoute::Anthropic,
        Vertical::XaiApi => ToolRoute::XaiApi,
        Vertical::OpenRouter => ToolRoute::OpenRouter,
    };
    let model = ModelId::new(model).map_err(|_| CliFailure::new("invalid template model"))?;
    let native_code_mode = super::policy::composition_requests_native_ptc(composition)
        && native
            .is_some_and(|provider| super::tools::is_codex_code_mode_candidate(provider, &model));
    if composition == CompositionSelection::NativePtc && !native_code_mode {
        return Err(CliFailure::new(
            "template has no enrolled native composition",
        ));
    }
    tools.admit_composition(composition)?;
    let plan = super::tools::bind_optional_tool_plan(tools, route, account.as_str())?;
    let extensions = ProductToolExtensions::empty().with_plan(&plan)?;
    let registry = super::tools::coding_tool_registry_with_extensions(
        "template-definition",
        composition,
        native_code_mode,
        &extensions,
    )?;
    let surface = product_surface_with_fence(
        JobConfig::default(),
        route.presentation(),
        registry,
        definition_target()?,
        &extensions,
        super::policy::occupancy_session_fence(access),
        super::tools::composition_runtime(composition, native_code_mode),
    )?;
    Ok(surface
        .projection()
        .entries()
        .iter()
        .map(|entry| {
            let extra = entry
                .canonical_operations()
                .values()
                .any(|identity| extensions.names().any(|name| name == identity.name()));
            TemplateTool {
                name: entry.exposed_name().to_string(),
                extra,
            }
        })
        .collect())
}

fn definition_target() -> CliResult<ProviderModelProjectionIdentityV2> {
    // The compiler stamps projection identities even though this read only
    // consumes the menu. This target is never used to admit a request.
    let digest = Digest::sha256(
        DigestDomain::EffectiveProviderConfiguration,
        5,
        &[b"template-definition"],
    )
    .map_err(|_| CliFailure::new("invalid definition projection digest"))?;
    let identity = serde_json::to_value(digest)
        .and_then(serde_json::from_value::<EffectiveProviderConfigurationId>)
        .map_err(|_| CliFailure::new("invalid definition projection identity"))?;
    ProviderModelProjectionIdentityV2::new(
        IntegrationClass::Custom,
        "definition",
        "definition",
        "definition",
        "definition",
        "definition",
        "definition",
        identity,
        PROCESS_TOOL_PROJECTION_REVISION,
    )
    .map_err(|_| CliFailure::new("invalid definition projection target"))
}
