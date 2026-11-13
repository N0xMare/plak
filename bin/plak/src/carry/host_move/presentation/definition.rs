//! Exact semantic schemas and exposure, with target-recomputed content identities.
#[cfg(test)]
use plakous::model::ToolExecutionOwner;
use plakous::model::{
    ConfiguredTool, ToolCallerDisposition, ToolConcurrency, ToolDefinition, ToolEffect,
    ToolExecutionDisposition, ToolInputContract, ToolOutputContract,
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Definition {
    name: String,
    semantic_version: String,
    description: String,
    input: Input,
    output: Output,
    concurrency: Concurrency,
    read_only: bool,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Exposure {
    definition: Definition,
    execution: ToolExecutionDisposition,
    caller: ToolCallerDisposition,
    mcp_server: Option<String>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Input {
    StructuredV2 { schema: serde_json::Value },
    Utf8 { bytes: u32 },
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Output {
    StructuredV1 { schema: serde_json::Value },
    Utf8 { bytes: u32 },
    ModelContent { parts: u32, bytes: u32 },
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Concurrency {
    Serial,
    Parallel,
    ResourceExclusive { keys: Vec<String> },
}

impl Definition {
    #[cfg(test)]
    pub(super) fn capture(value: &ToolDefinition) -> Result<Self, String> {
        Ok(Self {
            name: value.name().as_str().into(),
            semantic_version: value.semantic_version().into(),
            description: value.description().into(),
            input: match value.input() {
                ToolInputContract::Structured(schema) => Input::StructuredV2 {
                    schema: schema.document().value().as_ref().clone(),
                },
                ToolInputContract::Utf8(limit) => Input::Utf8 {
                    bytes: limit.max_bytes(),
                },
                _ => return Err("unsupported move input contract".into()),
            },
            output: match value.output() {
                ToolOutputContract::Structured(schema) => Output::StructuredV1 {
                    schema: schema.document().value().as_ref().clone(),
                },
                ToolOutputContract::Utf8(limit) => Output::Utf8 {
                    bytes: limit.max_bytes(),
                },
                ToolOutputContract::ModelContent(limit) => Output::ModelContent {
                    parts: limit.max_parts(),
                    bytes: limit.max_bytes(),
                },
                _ => return Err("unsupported move output contract".into()),
            },
            concurrency: match value.concurrency() {
                ToolConcurrency::Serial => Concurrency::Serial,
                ToolConcurrency::Parallel => Concurrency::Parallel,
                ToolConcurrency::ResourceExclusive(keys) => Concurrency::ResourceExclusive {
                    keys: keys.iter().map(|key| key.as_str().into()).collect(),
                },
                _ => return Err("unsupported move tool concurrency".into()),
            },
            read_only: match value.effect() {
                ToolEffect::ReadOnly => true,
                ToolEffect::Mutating => false,
                _ => return Err("unsupported move tool effect".into()),
            },
        })
    }

    pub(super) fn decode(self) -> Result<ToolDefinition, String> {
        let input = match self.input {
            Input::StructuredV2 { schema } => ToolInputContract::structured(schema),
            Input::Utf8 { bytes } => ToolInputContract::utf8(bytes),
        }
        .map_err(|e| e.to_string())?;
        let output = match self.output {
            Output::StructuredV1 { schema } => ToolOutputContract::structured(schema),
            Output::Utf8 { bytes } => ToolOutputContract::utf8(bytes),
            Output::ModelContent { parts, bytes } => {
                ToolOutputContract::model_content(parts, bytes)
            }
        }
        .map_err(|e| e.to_string())?;
        let concurrency = match self.concurrency {
            Concurrency::Serial => ToolConcurrency::Serial,
            Concurrency::Parallel => ToolConcurrency::Parallel,
            Concurrency::ResourceExclusive { keys } => ToolConcurrency::ResourceExclusive(
                keys.into_iter()
                    .map(plakous::model::ToolResourceKey::new)
                    .collect::<Result<_, _>>()
                    .map_err(|e| e.to_string())?,
            ),
        };
        ToolDefinition::new(
            plakous::model::ToolName::new(self.name).map_err(|e| e.to_string())?,
            self.semantic_version,
            self.description,
            input,
            output,
            concurrency,
            if self.read_only {
                ToolEffect::ReadOnly
            } else {
                ToolEffect::Mutating
            },
        )
        .map_err(|e| e.to_string())
    }
}

impl Exposure {
    #[cfg(test)]
    pub(super) fn capture(value: &ConfiguredTool) -> Result<Self, String> {
        Ok(Self {
            definition: Definition::capture(value.definition())?,
            execution: value.execution(),
            caller: value.caller(),
            mcp_server: match value.local_owner() {
                Some(ToolExecutionOwner::Mcp { server }) => Some(server.as_str().into()),
                _ => None,
            },
        })
    }

    pub(super) fn decode(self) -> Result<ConfiguredTool, String> {
        let configured =
            ConfiguredTool::new(self.definition.decode()?, self.execution, self.caller);
        match self.mcp_server {
            Some(server) => configured
                .with_mcp_owner(
                    plakous::model::McpServerId::new(server).map_err(|e| e.to_string())?,
                )
                .map_err(|e| e.to_string()),
            None => Ok(configured),
        }
    }
}
