//! Listing an MCP server's tools and freezing the selected ones.

use std::collections::BTreeMap;
use std::sync::Arc;

use plakous_model::{Digest, DigestDomain, PortableToolSchemaV2, ToolName};
use plakous_tools::McpServerName;
use rmcp::model::Tool;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

use crate::error::{McpError, McpErrorKind};
use crate::lowering::lower_input_schema;
use crate::naming::exposed_tool_name;
use crate::spec::McpServerSpec;

/// Most tools one server listing may carry.
pub const MAX_LISTED_TOOLS: usize = 512;
/// Most encoded bytes one server listing may carry.
pub const MAX_LISTING_BYTES: usize = 4 * 1024 * 1024;
/// Longest frozen tool description; longer ones are cut with a marker.
pub const MAX_DESCRIPTION_BYTES: usize = 4 * 1024;
const DESCRIPTION_CUT_MARKER: &str = " [description truncated]";

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
    server: McpServerName,
    mcp_name: Arc<str>,
    exposed_name: ToolName,
    description: Arc<str>,
    input_schema: PortableToolSchemaV2,
    annotations: Option<Value>,
    definition_digest: Digest,
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
/// unlowerable tool, and any two selected tools whose exposed names collide.
pub(crate) fn freeze_listing(
    spec: &McpServerSpec,
    tools: &[Tool],
    selection: &McpToolSelection,
) -> Result<FrozenMcpListing, McpError> {
    let server = spec.name();
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

fn bounded_description(description: &str) -> String {
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
mod tests {
    use serde_json::json;

    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn tool(name: &str, schema: Value) -> Result<Tool, Box<dyn std::error::Error>> {
        let Value::Object(schema) = schema else {
            return Err("schema".into());
        };
        Ok(Tool::new(
            name.to_owned(),
            format!("{name} tool"),
            Arc::new(schema),
        ))
    }

    fn spec() -> Result<McpServerSpec, Box<dyn std::error::Error>> {
        Ok(McpServerSpec::stdio(
            McpServerName::new("fake")?,
            "fake",
            [],
        ))
    }

    fn object() -> Value {
        json!({"type": "object", "properties": {"q": {"type": "string"}}})
    }

    #[test]
    fn wildcard_selection_omits_unlowerable_tools_with_a_notice() -> TestResult {
        let tools = [
            tool("echo", object())?,
            tool(
                "map",
                json!({"type": "object", "additionalProperties": {"type": "string"}}),
            )?,
        ];
        let frozen = freeze_listing(&spec()?, &tools, &McpToolSelection::All)?;
        assert_eq!(frozen.records().len(), 1);
        assert_eq!(
            frozen.records()[0].exposed_name().as_str(),
            "mcp__fake__echo"
        );
        assert_eq!(frozen.records()[0].description(), "echo tool");
        assert_eq!(
            frozen.notices(),
            [
                "mcp tool fake/map: input schema is not lowerable: additionalProperties with a schema at /; omitted"
            ]
        );
        let named = freeze_listing(
            &spec()?,
            &tools,
            &McpToolSelection::Named(vec!["map".into()]),
        );
        assert_eq!(
            named.err().map(|error| error.kind()),
            Some(McpErrorKind::Selection)
        );
        Ok(())
    }

    #[test]
    fn named_selection_follows_its_order_and_respects_the_allowlist() -> TestResult {
        let tools = [
            tool("a", object())?,
            tool("b", object())?,
            tool("c", object())?,
        ];
        let spec = spec()?.with_allowlist(["a".to_owned(), "c".to_owned()]);
        let frozen = freeze_listing(
            &spec,
            &tools,
            &McpToolSelection::Named(vec!["c".into(), "a".into()]),
        )?;
        let names = frozen
            .records()
            .iter()
            .map(McpToolRecord::mcp_name)
            .collect::<Vec<_>>();
        assert_eq!(names, ["c", "a"]);
        let all = freeze_listing(&spec, &tools, &McpToolSelection::All)?;
        assert_eq!(all.records().len(), 2);
        for (name, message) in [
            ("b", "mcp tool fake/b is not in the server's allowlist"),
            ("z", "mcp tool fake/z is not in the server's allowlist"),
        ] {
            let error = freeze_listing(&spec, &tools, &McpToolSelection::Named(vec![name.into()]))
                .err()
                .ok_or("selection bound")?;
            assert_eq!(error.message(), message);
        }
        let open = freeze_listing(
            &self::spec()?,
            &tools,
            &McpToolSelection::Named(vec!["z".into()]),
        )
        .err()
        .ok_or("selection bound")?;
        assert_eq!(open.message(), "mcp server fake has no tool z");
        Ok(())
    }

    #[test]
    fn names_colliding_after_mapping_refuse_the_bind_naming_both() -> TestResult {
        let tools = [tool("a.b", object())?, tool("a/b", object())?];
        let error = freeze_listing(&spec()?, &tools, &McpToolSelection::All)
            .err()
            .ok_or("collision bound")?;
        assert_eq!(
            error.message(),
            "mcp tools fake/a.b and fake/a/b both expose as mcp__fake__a_b"
        );
        Ok(())
    }

    #[test]
    fn long_descriptions_are_cut_with_a_marker() -> TestResult {
        let mut long = tool("long", object())?;
        long.description = Some("\u{e9}".repeat(4000).into());
        let frozen = freeze_listing(&spec()?, &[long], &McpToolSelection::All)?;
        let description = frozen.records()[0].description();
        assert!(description.len() <= MAX_DESCRIPTION_BYTES);
        assert!(description.ends_with(DESCRIPTION_CUT_MARKER));
        Ok(())
    }

    #[test]
    fn the_tool_digest_ignores_key_order_and_tracks_schema_edits() -> TestResult {
        let server = McpServerName::new("fake")?;
        let first = tool(
            "t",
            json!({"type": "object", "properties": {"a": {"type": "string"}, "b": {"type": "integer"}}}),
        )?;
        let reordered = tool(
            "t",
            json!({"properties": {"b": {"type": "integer"}, "a": {"type": "string"}}, "type": "object"}),
        )?;
        let edited = tool(
            "t",
            json!({"type": "object", "properties": {"a": {"type": "integer"}}}),
        )?;
        assert_eq!(
            listed_tool_digest(&server, &first)?,
            listed_tool_digest(&server, &reordered)?
        );
        assert_ne!(
            listed_tool_digest(&server, &first)?,
            listed_tool_digest(&server, &edited)?
        );
        Ok(())
    }
}
