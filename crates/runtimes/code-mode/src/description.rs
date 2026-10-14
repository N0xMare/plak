use crate::ToolDefinition;
use crate::ToolNamespaceDescription;
use codex_code_mode_protocol::ImageDetailVisibility;
use std::collections::BTreeMap;

/// Exact parsed form of raw `exec` tool input.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParsedExecSource {
    /// JavaScript source after removing the optional first-line pragma.
    pub code: String,
    /// Optional pragma yield interval.
    pub yield_time_ms: Option<u64>,
    /// Optional pragma output-token bound.
    pub max_output_tokens: Option<usize>,
}

/// Inputs to the pinned upstream exec-description renderer.
pub struct ExecDescription<'a> {
    /// Immediately enabled nested tools.
    pub enabled_tools: &'a [ToolDefinition],
    /// Deferred nested tools named in metadata but omitted from declarations.
    pub deferred_tools: &'a [ToolDefinition],
    /// Shared descriptions keyed by namespace identity.
    pub namespace_descriptions: &'a BTreeMap<String, ToolNamespaceDescription>,
    /// Default initial yield interval shown to the model.
    pub default_exec_yield_time_ms: u64,
    /// Whether direct provider-visible tools are absent.
    pub code_mode_only: bool,
    /// Whether the legacy image-detail helper is model-visible.
    pub image_detail_visible: bool,
}

/// Parses raw exec input using the exact pinned upstream grammar.
pub fn parse_exec_source(input: &str) -> Result<ParsedExecSource, String> {
    codex_code_mode_protocol::parse_exec_source(input).map(|parsed| ParsedExecSource {
        code: parsed.code,
        yield_time_ms: parsed.yield_time_ms,
        max_output_tokens: parsed.max_output_tokens,
    })
}

/// Renders the exact pinned upstream exec tool description.
pub fn build_exec_description(input: &ExecDescription<'_>) -> String {
    let enabled = input
        .enabled_tools
        .iter()
        .map(ToolDefinition::to_upstream)
        .collect::<Vec<_>>();
    let deferred = input
        .deferred_tools
        .iter()
        .map(ToolDefinition::to_upstream)
        .collect::<Vec<_>>();
    let namespaces = input
        .namespace_descriptions
        .iter()
        .map(|(key, value)| {
            (
                key.clone(),
                codex_code_mode_protocol::ToolNamespaceDescription {
                    name: value.name.clone(),
                    description: value.description.clone(),
                },
            )
        })
        .collect();
    codex_code_mode_protocol::build_exec_tool_description(
        &enabled,
        &deferred,
        &namespaces,
        input.default_exec_yield_time_ms,
        input.code_mode_only,
        if input.image_detail_visible {
            ImageDetailVisibility::Visible
        } else {
            ImageDetailVisibility::Hidden
        },
    )
}

/// Returns the exact pinned upstream wait tool description.
pub fn build_wait_description() -> &'static str {
    codex_code_mode_protocol::build_wait_tool_description()
}
