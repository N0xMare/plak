use std::{collections::BTreeMap, path::Path, sync::Arc};

use plakous::mcp::McpToolRecord;
use plakous::model::ToolName;
use plakous::tools::{McpServerName, ToolRegistration};

use super::NamedSecrets;
use crate::tool_plan::TemplateToolSelection;

pub(crate) struct BoundMcpTools {
    pub(crate) names: Vec<(McpServerName, ToolName)>,
    pub(crate) registrations: Vec<ToolRegistration>,
    pub(crate) notices: Vec<String>,
    pub(crate) records: Vec<McpToolRecord>,
}

pub(crate) fn bind_mcp_tools(
    selection: &TemplateToolSelection,
    _tool_sections: &BTreeMap<String, String>,
    _workspace_root: &Path,
    _secrets: Arc<dyn NamedSecrets>,
    _frozen: Option<&[McpToolRecord]>,
) -> Result<BoundMcpTools, crate::CliFailure> {
    crate::capabilities::require_tools(selection).map_err(|error| error.to_string())?;
    Ok(BoundMcpTools {
        names: Vec::new(),
        registrations: Vec::new(),
        notices: Vec::new(),
        records: Vec::new(),
    })
}

pub(crate) fn recorded_mcp_tools(
    selection: &TemplateToolSelection,
    _tool_sections: &BTreeMap<String, String>,
    _secrets: &Arc<dyn NamedSecrets>,
    _frozen: Option<&[McpToolRecord]>,
) -> Result<BoundMcpTools, crate::CliFailure> {
    bind_mcp_tools(
        selection,
        &BTreeMap::new(),
        Path::new(""),
        Arc::new(super::NoNamedSecrets),
        None,
    )
}
