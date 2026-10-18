use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use std::{fmt, path::PathBuf};
use thiserror::Error;

pub const MAX_SOURCE_BYTES: usize = 1_048_576;
pub const MAX_ID_BYTES: usize = 256;
pub const MAX_TOOL_NAME_BYTES: usize = 128;
pub const MAX_TOOL_DESCRIPTION_BYTES: usize = 16_384;
pub const MAX_TOOL_SCHEMA_BYTES: usize = 65_536;
pub const MAX_CATALOG_TOOLS: usize = 7;
pub const MAX_PRODUCT_CATALOG_TOOLS: usize = 64;
pub const V1_NESTED_TOOL_NAMES: [&str; MAX_CATALOG_TOOLS] = [
    "exec_command",
    "write_stdin",
    "apply_patch",
    "update_plan",
    "read_file",
    "search_repository",
    "inspect_image",
];
pub const MAX_CATALOG_BYTES: usize = 262_144;
/// Largest integer exactly representable by JavaScript, matching the exec pragma parser.
pub const MAX_YIELD_TIME_MS: u64 = (1_u64 << 53) - 1;
/// Largest output-token value encodable by the pinned V1 host wire protocol.
pub const MAX_OUTPUT_TOKENS: usize = i32::MAX as usize;
pub const MAX_NESTED_INPUT_BYTES: usize = 1_048_576;
pub const MAX_NESTED_OUTPUT_BYTES: usize = 1_048_576;
pub const MAX_NOTIFICATION_BYTES: usize = 65_536;
pub const MAX_RUNTIME_RESPONSE_BYTES: usize = 4_194_304;

#[derive(Debug, Error)]
pub enum SidecarError {
    #[error("failed to inspect code-mode host {path}: {source}")]
    HostIo {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("code-mode host {path} is unavailable: {reason}")]
    HostUnavailable { path: PathBuf, reason: String },
    #[error("invalid code-mode {field}: {reason}")]
    InvalidInput { field: &'static str, reason: String },
    #[error("code-mode {operation} failed: {reason}")]
    Protocol {
        operation: &'static str,
        reason: String,
    },
    #[error("code-mode session was lost during {operation}: {reason}")]
    SessionLost {
        operation: &'static str,
        reason: String,
    },
}

impl SidecarError {
    pub(crate) fn invalid(field: &'static str, reason: impl Into<String>) -> Self {
        Self::InvalidInput {
            field,
            reason: reason.into(),
        }
    }
}

fn bounded_nonempty(
    field: &'static str,
    value: String,
    max: usize,
) -> Result<String, SidecarError> {
    if value.is_empty() {
        return Err(SidecarError::invalid(field, "must not be empty"));
    }
    if value.len() > max {
        return Err(SidecarError::invalid(
            field,
            format!("exceeds {max} UTF-8 bytes"),
        ));
    }
    Ok(value)
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct CellId(String);
impl CellId {
    pub fn try_new(value: impl Into<String>) -> Result<Self, SidecarError> {
        bounded_nonempty("cell ID", value.into(), MAX_ID_BYTES).map(Self)
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl fmt::Display for CellId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct ToolName {
    name: String,
    namespace: Option<String>,
}
impl ToolName {
    pub fn try_new(
        namespace: Option<String>,
        name: impl Into<String>,
    ) -> Result<Self, SidecarError> {
        let name = bounded_nonempty("tool name", name.into(), MAX_TOOL_NAME_BYTES)?;
        let namespace = namespace
            .map(|v| bounded_nonempty("tool namespace", v, MAX_TOOL_NAME_BYTES))
            .transpose()?;
        Ok(Self { name, namespace })
    }
    pub fn plain(name: impl Into<String>) -> Result<Self, SidecarError> {
        Self::try_new(None, name)
    }
    pub fn namespaced(
        namespace: impl Into<String>,
        name: impl Into<String>,
    ) -> Result<Self, SidecarError> {
        Self::try_new(Some(namespace.into()), name)
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn namespace(&self) -> Option<&str> {
        self.namespace.as_deref()
    }
    pub(crate) fn to_upstream(&self) -> plakous_codex_code_mode_protocol::ToolName {
        plakous_codex_code_mode_protocol::ToolName::new(self.namespace.clone(), self.name.clone())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CodeModeToolKind {
    Function,
    Freeform,
}
impl CodeModeToolKind {
    pub(crate) fn to_upstream(self) -> plakous_codex_code_mode_protocol::CodeModeToolKind {
        match self {
            Self::Function => plakous_codex_code_mode_protocol::CodeModeToolKind::Function,
            Self::Freeform => plakous_codex_code_mode_protocol::CodeModeToolKind::Freeform,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ToolDefinition {
    name: String,
    tool_name: ToolName,
    description: String,
    kind: CodeModeToolKind,
    input_schema: Option<JsonValue>,
    output_schema: Option<JsonValue>,
}
impl ToolDefinition {
    pub fn try_new(
        name: impl Into<String>,
        tool_name: ToolName,
        description: impl Into<String>,
        kind: CodeModeToolKind,
        input_schema: Option<JsonValue>,
        output_schema: Option<JsonValue>,
    ) -> Result<Self, SidecarError> {
        let name = bounded_nonempty("catalog key", name.into(), MAX_TOOL_NAME_BYTES)?;
        let description = bounded_nonempty(
            "tool description",
            description.into(),
            MAX_TOOL_DESCRIPTION_BYTES,
        )?;
        validate_json_bound("input schema", input_schema.as_ref(), MAX_TOOL_SCHEMA_BYTES)?;
        validate_json_bound(
            "output schema",
            output_schema.as_ref(),
            MAX_TOOL_SCHEMA_BYTES,
        )?;
        match kind {
            CodeModeToolKind::Function if input_schema.is_none() => {
                return Err(SidecarError::invalid(
                    "input schema",
                    "function tools require a schema",
                ));
            }
            CodeModeToolKind::Freeform if input_schema.is_some() => {
                return Err(SidecarError::invalid(
                    "input schema",
                    "freeform tools must not carry a JSON schema",
                ));
            }
            _ => {}
        }
        Ok(Self {
            name,
            tool_name,
            description,
            kind,
            input_schema,
            output_schema,
        })
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn tool_name(&self) -> &ToolName {
        &self.tool_name
    }
    pub fn description(&self) -> &str {
        &self.description
    }
    pub fn kind(&self) -> CodeModeToolKind {
        self.kind
    }
    pub fn input_schema(&self) -> Option<&JsonValue> {
        self.input_schema.as_ref()
    }
    pub fn output_schema(&self) -> Option<&JsonValue> {
        self.output_schema.as_ref()
    }
    pub(crate) fn to_upstream(&self) -> plakous_codex_code_mode_protocol::ToolDefinition {
        plakous_codex_code_mode_protocol::ToolDefinition {
            name: self.name.clone(),
            tool_name: self.tool_name.to_upstream(),
            description: self.description.clone(),
            kind: self.kind.to_upstream(),
            input_schema: self.input_schema.clone(),
            output_schema: self.output_schema.clone(),
        }
    }
}

fn validate_json_bound(
    field: &'static str,
    value: Option<&JsonValue>,
    max: usize,
) -> Result<(), SidecarError> {
    if value.is_some_and(|v| serde_json::to_vec(v).is_ok_and(|b| b.len() > max)) {
        return Err(SidecarError::invalid(
            field,
            format!("exceeds {max} encoded bytes"),
        ));
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolNamespaceDescription {
    pub name: String,
    pub description: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutionLimits {
    max_yield_time_ms: u64,
    max_output_tokens: usize,
}
impl ExecutionLimits {
    pub fn try_new(max_yield_time_ms: u64, max_output_tokens: usize) -> Result<Self, SidecarError> {
        if max_yield_time_ms > MAX_YIELD_TIME_MS {
            return Err(SidecarError::invalid(
                "maximum yield",
                format!("must be 0..={MAX_YIELD_TIME_MS}"),
            ));
        }
        if max_output_tokens > MAX_OUTPUT_TOKENS {
            return Err(SidecarError::invalid(
                "maximum output tokens",
                format!("must be 0..={MAX_OUTPUT_TOKENS}"),
            ));
        }
        Ok(Self {
            max_yield_time_ms,
            max_output_tokens,
        })
    }
    pub fn max_yield_time_ms(&self) -> u64 {
        self.max_yield_time_ms
    }
    pub fn max_output_tokens(&self) -> usize {
        self.max_output_tokens
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ExecuteRequest {
    tool_call_id: String,
    enabled_tools: Vec<ToolDefinition>,
    source: String,
    yield_time_ms: u64,
    max_output_tokens: usize,
}
impl ExecuteRequest {
    pub fn try_new(
        tool_call_id: impl Into<String>,
        enabled_tools: Vec<ToolDefinition>,
        source: impl Into<String>,
        yield_time_ms: u64,
        max_output_tokens: usize,
    ) -> Result<Self, SidecarError> {
        let tool_call_id =
            bounded_nonempty("outer tool-call ID", tool_call_id.into(), MAX_ID_BYTES)?;
        let source = bounded_nonempty("source", source.into(), MAX_SOURCE_BYTES)?;
        ExecutionLimits::try_new(yield_time_ms, max_output_tokens)?;
        validate_catalog(&enabled_tools)?;
        Ok(Self {
            tool_call_id,
            enabled_tools,
            source,
            yield_time_ms,
            max_output_tokens,
        })
    }
    pub fn try_new_bounded_catalog(
        tool_call_id: impl Into<String>,
        enabled_tools: Vec<ToolDefinition>,
        source: impl Into<String>,
        yield_time_ms: u64,
        max_output_tokens: usize,
    ) -> Result<Self, SidecarError> {
        let tool_call_id =
            bounded_nonempty("outer tool-call ID", tool_call_id.into(), MAX_ID_BYTES)?;
        let source = bounded_nonempty("source", source.into(), MAX_SOURCE_BYTES)?;
        ExecutionLimits::try_new(yield_time_ms, max_output_tokens)?;
        validate_bounded_catalog(&enabled_tools)?;
        Ok(Self {
            tool_call_id,
            enabled_tools,
            source,
            yield_time_ms,
            max_output_tokens,
        })
    }
    pub fn tool_call_id(&self) -> &str {
        &self.tool_call_id
    }
    pub fn enabled_tools(&self) -> &[ToolDefinition] {
        &self.enabled_tools
    }
    pub fn source(&self) -> &str {
        &self.source
    }
    pub fn yield_time_ms(&self) -> u64 {
        self.yield_time_ms
    }
    pub fn max_output_tokens(&self) -> usize {
        self.max_output_tokens
    }
}

pub(crate) fn validate_catalog(tools: &[ToolDefinition]) -> Result<(), SidecarError> {
    if tools.len() != MAX_CATALOG_TOOLS {
        return Err(SidecarError::invalid(
            "catalog",
            format!("must contain exactly {MAX_CATALOG_TOOLS} tools"),
        ));
    }
    let bytes = serde_json::to_vec(tools)
        .map_err(|e| SidecarError::invalid("catalog", e.to_string()))?
        .len();
    if bytes > MAX_CATALOG_BYTES {
        return Err(SidecarError::invalid(
            "catalog",
            format!("exceeds {MAX_CATALOG_BYTES} encoded bytes"),
        ));
    }
    let mut keys = std::collections::BTreeSet::new();
    let mut identities = std::collections::BTreeSet::new();
    for (tool, expected_name) in tools.iter().zip(V1_NESTED_TOOL_NAMES) {
        if tool.name != expected_name {
            return Err(SidecarError::invalid(
                "catalog",
                format!(
                    "expected frozen V1 tool {expected_name}, found {}",
                    tool.name
                ),
            ));
        }
        if !keys.insert(tool.name.clone()) {
            return Err(SidecarError::invalid(
                "catalog",
                format!("duplicate key {}", tool.name),
            ));
        }
        if !identities.insert(tool.tool_name.clone()) {
            return Err(SidecarError::invalid(
                "catalog",
                "duplicate canonical tool identity",
            ));
        }
    }
    Ok(())
}

fn validate_bounded_catalog(tools: &[ToolDefinition]) -> Result<(), SidecarError> {
    if tools.is_empty() || tools.len() > MAX_PRODUCT_CATALOG_TOOLS {
        return Err(SidecarError::invalid(
            "catalog",
            format!("must contain 1..={MAX_PRODUCT_CATALOG_TOOLS} tools"),
        ));
    }
    let bytes = serde_json::to_vec(tools)
        .map_err(|error| SidecarError::invalid("catalog", error.to_string()))?
        .len();
    if bytes > MAX_CATALOG_BYTES {
        return Err(SidecarError::invalid(
            "catalog",
            format!("exceeds {MAX_CATALOG_BYTES} encoded bytes"),
        ));
    }
    let mut keys = std::collections::BTreeSet::new();
    let mut identities = std::collections::BTreeSet::new();
    for tool in tools {
        if !keys.insert(tool.name.clone()) {
            return Err(SidecarError::invalid(
                "catalog",
                format!("duplicate key {}", tool.name),
            ));
        }
        if !identities.insert(tool.tool_name.clone()) {
            return Err(SidecarError::invalid(
                "catalog",
                "duplicate canonical tool identity",
            ));
        }
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WaitRequest {
    cell_id: CellId,
    yield_time_ms: u64,
}
impl WaitRequest {
    pub fn try_new(cell_id: CellId, yield_time_ms: u64) -> Result<Self, SidecarError> {
        if yield_time_ms > MAX_YIELD_TIME_MS {
            return Err(SidecarError::invalid(
                "wait yield",
                format!("must be 0..={MAX_YIELD_TIME_MS}"),
            ));
        }
        Ok(Self {
            cell_id,
            yield_time_ms,
        })
    }
    pub fn cell_id(&self) -> &CellId {
        &self.cell_id
    }
    pub fn yield_time_ms(&self) -> u64 {
        self.yield_time_ms
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ImageDetail {
    Auto,
    Low,
    High,
    Original,
}
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentItem {
    InputText {
        text: String,
    },
    InputImage {
        image_url: String,
        detail: Option<ImageDetail>,
    },
    InputAudio {
        audio_url: String,
    },
}
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum RuntimeResponse {
    Yielded {
        cell_id: CellId,
        content_items: Vec<ContentItem>,
    },
    Terminated {
        cell_id: CellId,
        content_items: Vec<ContentItem>,
    },
    Result {
        cell_id: CellId,
        content_items: Vec<ContentItem>,
        error_text: Option<String>,
    },
}
#[derive(Clone, Debug, PartialEq)]
pub enum WaitOutcome {
    LiveCell(RuntimeResponse),
    MissingCell(RuntimeResponse),
}
#[derive(Clone, Debug, PartialEq)]
pub struct NestedToolCall {
    pub cell_id: CellId,
    pub runtime_tool_call_id: String,
    pub tool_name: ToolName,
    pub tool_kind: CodeModeToolKind,
    pub input: Option<JsonValue>,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Notification {
    pub call_id: String,
    pub cell_id: CellId,
    pub text: String,
}
