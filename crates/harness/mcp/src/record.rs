//! Credential-free frozen MCP tool vocabulary and its unchanged encoding.

use std::sync::Arc;

use plakous_model::{Digest, PortableToolSchemaV2, ToolName};
use plakous_tools::McpServerName;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

use super::naming::exposed_tool_name;

/// Longest frozen tool description; longer ones are cut with a marker.
pub const MAX_DESCRIPTION_BYTES: usize = 4 * 1024;

/// Which listed tools a template selects from one server.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum McpToolSelection {
    /// Every allowed tool; a tool without a portable schema is omitted with a
    /// notice.
    All,
    /// Exactly these MCP tool names, in this order; any that is missing,
    /// not allowed or not lowerable refuses the bind.
    Named(Vec<String>),
}

/// One selected tool, frozen at bind.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct McpToolRecord {
    pub(super) server: McpServerName,
    pub(super) mcp_name: Arc<str>,
    pub(super) exposed_name: ToolName,
    pub(super) description: Arc<str>,
    pub(super) input_schema: PortableToolSchemaV2,
    pub(super) annotations: Option<Value>,
    pub(super) definition_digest: Digest,
}

impl McpToolRecord {
    /// Returns the server id.
    #[must_use]
    pub const fn server(&self) -> &McpServerName {
        &self.server
    }

    /// Returns the tool's name on the server.
    #[must_use]
    pub fn mcp_name(&self) -> &str {
        &self.mcp_name
    }

    /// Returns the model-visible name.
    #[must_use]
    pub const fn exposed_name(&self) -> &ToolName {
        &self.exposed_name
    }

    /// Returns the bounded description.
    #[must_use]
    pub fn description(&self) -> &str {
        &self.description
    }

    /// Returns the lowered input schema.
    #[must_use]
    pub const fn input_schema(&self) -> &PortableToolSchemaV2 {
        &self.input_schema
    }

    /// Returns the server's annotations as listed. They are hints from an
    /// untrusted server and decide nothing.
    #[must_use]
    pub const fn annotations(&self) -> Option<&Value> {
        self.annotations.as_ref()
    }

    /// Returns the digest of the listed name, description and input schema.
    #[must_use]
    pub const fn definition_digest(&self) -> Digest {
        self.definition_digest
    }
}

/// The encoding an embedder persists so an exact reopen rebuilds the same
/// tools without listing the server again.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct McpToolRecordWire {
    server: String,
    mcp_name: String,
    exposed_name: ToolName,
    description: String,
    input_schema: PortableToolSchemaV2,
    annotations: Option<Value>,
    definition_digest: Digest,
}

impl Serialize for McpToolRecord {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        McpToolRecordWire {
            server: self.server.as_str().to_owned(),
            mcp_name: self.mcp_name.to_string(),
            exposed_name: self.exposed_name.clone(),
            description: self.description.to_string(),
            input_schema: self.input_schema.clone(),
            annotations: self.annotations.clone(),
            definition_digest: self.definition_digest,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for McpToolRecord {
    /// Refuses a record whose server id is invalid, whose exposed name is not
    /// the one its server and MCP name expose, or whose description is over
    /// the bound.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let wire = McpToolRecordWire::deserialize(deserializer)?;
        let server = McpServerName::new(wire.server).map_err(D::Error::custom)?;
        let exposed = exposed_tool_name(&server, &wire.mcp_name).map_err(D::Error::custom)?;
        if exposed != wire.exposed_name {
            return Err(D::Error::custom(
                "mcp tool record exposed name does not match its server and tool",
            ));
        }
        if wire.description.len() > MAX_DESCRIPTION_BYTES {
            return Err(D::Error::custom(
                "mcp tool record description is over the bound",
            ));
        }
        Ok(Self {
            server,
            mcp_name: Arc::from(wire.mcp_name),
            exposed_name: exposed,
            description: Arc::from(wire.description),
            input_schema: wire.input_schema,
            annotations: wire.annotations,
            definition_digest: wire.definition_digest,
        })
    }
}
