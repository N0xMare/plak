//! Listing an MCP server's tools and freezing the selected ones.

use std::collections::BTreeMap;
use std::sync::Arc;

use plakous_model::{Digest, DigestDomain, ToolName};
use plakous_tools::McpServerName;
use rmcp::model::Tool;
use serde_json::Value;

use crate::surface::error::{McpError, McpErrorKind};
use crate::surface::lowering::lower_input_schema;
use crate::surface::naming::exposed_tool_name;
use crate::surface::resources::READ_RESOURCE;
use crate::surface::spec::McpServerSpec;

/// Most tools one server listing may carry.
pub const MAX_LISTED_TOOLS: usize = 512;
/// Most encoded bytes one server listing may carry.
pub const MAX_LISTING_BYTES: usize = 4 * 1024 * 1024;
const DESCRIPTION_CUT_MARKER: &str = " [description truncated]";

pub use super::record::{MAX_DESCRIPTION_BYTES, McpToolRecord, McpToolSelection};

/// The frozen selection of one server, with notices for omitted tools.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FrozenMcpListing {
    records: Vec<McpToolRecord>,
    notices: Vec<String>,
}

impl FrozenMcpListing {
    /// Returns the selected tools in presentation order.
    #[must_use]
    pub fn records(&self) -> &[McpToolRecord] {
        &self.records
    }

    /// Returns one operator line per tool a wildcard selection omitted.
    #[must_use]
    pub fn notices(&self) -> &[String] {
        &self.notices
    }
}

/// The digest of one listed tool's name, description and input schema.
pub(crate) fn listed_tool_digest(server: &McpServerName, tool: &Tool) -> Result<Digest, McpError> {
    let mut document = serde_json::Map::new();
    document.insert("name".into(), Value::String(tool.name.to_string()));
    if let Some(description) = &tool.description {
        document.insert("description".into(), Value::String(description.to_string()));
    }
    document.insert(
        "inputSchema".into(),
        Value::Object(tool.input_schema.as_ref().clone()),
    );
    let mut canonical = Vec::new();
    write_canonical(&Value::Object(document), &mut canonical);
    Digest::sha256(
        DigestDomain::ToolDefinition,
        1,
        &[
            b"plakous-mcp/listed-tool",
            server.as_str().as_bytes(),
            &canonical,
        ],
    )
    .map_err(|error| {
        McpError::with_source(
            McpErrorKind::Listing,
            format!("mcp server {server} tool digest failed"),
            error,
        )
    })
}

/// Sorted-key compact JSON, independent of the map's insertion order.
fn write_canonical(value: &Value, output: &mut Vec<u8>) {
    match value {
        Value::Object(object) => {
            output.push(b'{');
            let mut entries = object.iter().collect::<Vec<_>>();
            entries.sort_by(|left, right| left.0.cmp(right.0));
            for (index, (key, value)) in entries.into_iter().enumerate() {
                if index > 0 {
                    output.push(b',');
                }
                write_canonical(&Value::String(key.clone()), output);
                output.push(b':');
                write_canonical(value, output);
            }
            output.push(b'}');
        }
        Value::Array(values) => {
            output.push(b'[');
            for (index, value) in values.iter().enumerate() {
                if index > 0 {
                    output.push(b',');
                }
                write_canonical(value, output);
            }
            output.push(b']');
        }
        scalar => output.extend_from_slice(scalar.to_string().as_bytes()),
    }
}

/// Filters a listing by the server allowlist and the template selection,
/// then lowers, names and freezes each selected tool.
///
/// # Errors
///
/// Refuses an explicit selection that names a missing, disallowed or
/// unlowerable tool, any two selected tools whose exposed names collide, and
/// a definition with resources enabled when the server does not advertise
/// them or a selected tool exposes as the resources tool.
pub(crate) fn freeze_listing(
    spec: &McpServerSpec,
    tools: &[Tool],
    advertises_resources: bool,
    selection: &McpToolSelection,
) -> Result<FrozenMcpListing, McpError> {
    let server = spec.name();
    if spec.resources() && !advertises_resources {
        return Err(McpError::new(
            McpErrorKind::Selection,
            format!(
                "mcp server {server} has resources enabled but does not advertise the resources capability"
            ),
        ));
    }
    let selected: Vec<&Tool> = match selection {
        McpToolSelection::All => tools
            .iter()
            .filter(|tool| spec.allows(&tool.name))
            .collect(),
        McpToolSelection::Named(names) => names
            .iter()
            .map(|name| {
                if !spec.allows(name) {
                    return Err(McpError::new(
                        McpErrorKind::Selection,
                        format!("mcp tool {server}/{name} is not in the server's allowlist"),
                    ));
                }
                tools
                    .iter()
                    .find(|tool| tool.name == name.as_str())
                    .ok_or_else(|| {
                        McpError::new(
                            McpErrorKind::Selection,
                            format!("mcp server {server} has no tool {name}"),
                        )
                    })
            })
            .collect::<Result<_, _>>()?,
    };
    let mut records = Vec::with_capacity(selected.len());
    let mut notices = Vec::new();
    let mut exposed: BTreeMap<ToolName, Arc<str>> = BTreeMap::new();
    let resources_tool = exposed_tool_name(server, READ_RESOURCE)?;
    for tool in selected {
        let input_schema = match lower_input_schema(&tool.input_schema) {
            Ok(schema) => schema,
            Err(refusal) => {
                let line = format!("mcp tool {server}/{}: {refusal}", tool.name);
                if matches!(selection, McpToolSelection::Named(_)) {
                    return Err(McpError::new(McpErrorKind::Selection, line));
                }
                notices.push(format!("{line}; omitted"));
                continue;
            }
        };
        let exposed_name = exposed_tool_name(server, &tool.name)?;
        let mcp_name: Arc<str> = Arc::from(tool.name.as_ref());
        if spec.resources() && exposed_name == resources_tool {
            return Err(McpError::new(
                McpErrorKind::Selection,
                format!(
                    "mcp tool {server}/{mcp_name} and the resources tool both expose as {}",
                    exposed_name.as_str()
                ),
            ));
        }
        if let Some(previous) = exposed.insert(exposed_name.clone(), Arc::clone(&mcp_name)) {
            return Err(McpError::new(
                McpErrorKind::Selection,
                format!(
                    "mcp tools {server}/{previous} and {server}/{mcp_name} both expose as {}",
                    exposed_name.as_str()
                ),
            ));
        }
        records.push(McpToolRecord {
            server: server.clone(),
            exposed_name,
            description: Arc::from(bounded_description(
                tool.description
                    .as_deref()
                    .filter(|description| !description.trim().is_empty())
                    .unwrap_or(&format!("{} (mcp server {server})", tool.name)),
            )),
            input_schema,
            annotations: tool
                .annotations
                .as_ref()
                .and_then(|annotations| serde_json::to_value(annotations).ok()),
            definition_digest: listed_tool_digest(server, tool)?,
            mcp_name,
        });
    }
    Ok(FrozenMcpListing { records, notices })
}

pub(crate) fn bounded_description(description: &str) -> String {
    if description.len() <= MAX_DESCRIPTION_BYTES {
        return description.to_owned();
    }
    let mut end = MAX_DESCRIPTION_BYTES - DESCRIPTION_CUT_MARKER.len();
    while !description.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{DESCRIPTION_CUT_MARKER}", &description[..end])
}

#[cfg(test)]
mod tests;
