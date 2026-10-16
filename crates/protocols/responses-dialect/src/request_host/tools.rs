//! Tool definition and tool-choice lowering for request bodies.

use super::{
    BTreeSet, CompiledToolProjectionV2, ConfiguredTool, CustomToolPolicy, FunctionStrictPolicy,
    FunctionToolChoiceWire, FunctionToolWire, HostedTool, ModelRequest, ProjectedToolDefinitionV2,
    ResponsesCanonicalTool, ResponsesRequestConfig, ResponsesRequestError,
    ResponsesRequestOptionError, ResponsesRequestPolicy, ToolCallerDisposition, ToolChoice,
    ToolDefinition, ToolInputContract, ToolName, Value, json,
};
use crate::{ResponsesCanonicalToolKind, ResponsesCanonicalToolOwner};

/// Reports whether a definition explicitly declares free-form UTF-8 input.
#[must_use]
pub fn is_string_custom_tool_definition(tool: &ToolDefinition) -> bool {
    matches!(tool.input(), ToolInputContract::Utf8(_))
}

/// Reports whether a canonical definition remains a provider-visible custom
/// tool after applying an exact V2 projection.
#[must_use]
pub fn is_unprojected_string_custom_tool_definition(
    request: &ModelRequest,
    tool: &ToolDefinition,
) -> bool {
    is_string_custom_tool_definition(tool)
        && !request.tool_projection().is_some_and(|projection| {
            projection
                .definitions()
                .iter()
                .any(|entry| entry.covers_canonical_name(tool.name()))
        })
}

/// Builds the exact provider-visible local-tool table used by canonical
/// Responses decoding after an optional V2 projection. Each tool carries the
/// owner its configured tool names, so a call decodes with it from the first
/// parse.
#[must_use]
pub fn canonical_local_tools(request: &ModelRequest) -> Vec<ResponsesCanonicalTool> {
    let tool = |name: &ToolName, custom: bool| {
        let owner = match request.local_tool_owner(name) {
            plakous_model::ToolExecutionOwner::Mcp { server } => {
                ResponsesCanonicalToolOwner::Mcp(server)
            }
            _ => ResponsesCanonicalToolOwner::Local,
        };
        let kind = if custom {
            ResponsesCanonicalToolKind::Custom
        } else {
            ResponsesCanonicalToolKind::Function
        };
        ResponsesCanonicalTool::new(name.clone(), kind, owner)
    };
    if let Some(projection) = request.tool_projection() {
        return projection
            .definitions()
            .iter()
            .map(|entry| {
                tool(
                    entry.exposed_name(),
                    matches!(
                        entry.presentation(),
                        plakous_model::ProjectedToolPresentationV2::CustomUtf8 { .. }
                    ),
                )
            })
            .collect();
    }
    request
        .tools()
        .iter()
        .map(|configured| {
            let canonical = configured.definition();
            tool(
                canonical.name(),
                is_string_custom_tool_definition(canonical),
            )
        })
        .collect()
}

pub(super) fn lower_hosted_tool(hosted: HostedTool) -> Value {
    match hosted {
        HostedTool::ProgrammaticComposition => json!({"type":"programmatic_tool_calling"}),
        HostedTool::CodeInterpreter => json!({"type":"code_interpreter"}),
    }
}

pub(super) fn lower_tool_definition(
    configured: &ConfiguredTool,
    policy: ResponsesRequestPolicy,
    custom_tools: &BTreeSet<ToolName>,
) -> Result<Value, ResponsesRequestError> {
    let tool = configured.definition();
    if custom_tools.contains(tool.name()) {
        if policy.custom_tools == CustomToolPolicy::Reject {
            return Err(ResponsesRequestOptionError::CustomTools.into());
        }
        let mut wire = json!({
            "type": "custom",
            "name": tool.name().as_str(),
            "description": tool.description(),
        });
        lower_allowed_callers(&mut wire, configured.caller())?;
        return Ok(wire);
    }
    let mut wire = match policy.function_strict {
        FunctionStrictPolicy::IncludeTrue => FunctionToolWire::openai_strict(
            tool.name().as_str(),
            tool.description(),
            match tool.input() {
                ToolInputContract::Structured(schema) => schema.wire_document(),
                ToolInputContract::Utf8(_) => {
                    return Err(ResponsesRequestOptionError::CustomTools.into());
                }
                _ => return Err(ResponsesRequestOptionError::CustomTools.into()),
            },
        ),
        FunctionStrictPolicy::IncludeFalse
        | FunctionStrictPolicy::IncludeNull
        | FunctionStrictPolicy::Omit => FunctionToolWire::function(
            tool.name().as_str(),
            tool.description(),
            match tool.input() {
                ToolInputContract::Structured(schema) => schema.wire_document(),
                ToolInputContract::Utf8(_) => {
                    return Err(ResponsesRequestOptionError::CustomTools.into());
                }
                _ => return Err(ResponsesRequestOptionError::CustomTools.into()),
            },
        ),
    }
    .to_value();
    if matches!(policy.function_strict, FunctionStrictPolicy::IncludeNull) {
        wire.as_object_mut()
            .ok_or(ResponsesRequestError::Serialization)?
            .insert("strict".to_owned(), Value::Null);
    }
    if matches!(policy.function_strict, FunctionStrictPolicy::IncludeFalse) {
        wire["strict"] = Value::Bool(false);
    }
    lower_allowed_callers(&mut wire, configured.caller())?;
    Ok(wire)
}

pub(super) fn lower_allowed_callers(
    wire: &mut Value,
    caller: ToolCallerDisposition,
) -> Result<(), ResponsesRequestError> {
    let callers = match caller {
        ToolCallerDisposition::DirectOnly => return Ok(()),
        ToolCallerDisposition::ProviderProgrammaticOnly => json!(["programmatic"]),
        ToolCallerDisposition::DirectAndProviderProgrammatic => {
            json!(["direct", "programmatic"])
        }
    };
    wire.as_object_mut()
        .ok_or(ResponsesRequestError::Serialization)?
        .insert("allowed_callers".to_owned(), callers);
    Ok(())
}

pub(super) fn lower_projected_tool_definition(
    tool: &ProjectedToolDefinitionV2,
    policy: ResponsesRequestPolicy,
) -> Result<Value, ResponsesRequestError> {
    match tool.presentation() {
        plakous_model::ProjectedToolPresentationV2::StructuredFunction { input_schema } => {
            let mut wire = json!({
                "type": "function",
                "name": tool.exposed_name().as_str(),
                "description": tool.description(),
                "parameters": input_schema.wire_document()
            });
            match policy.function_strict {
                FunctionStrictPolicy::IncludeTrue => wire["strict"] = Value::Bool(true),
                FunctionStrictPolicy::IncludeFalse => wire["strict"] = Value::Bool(false),
                FunctionStrictPolicy::IncludeNull => wire["strict"] = Value::Null,
                FunctionStrictPolicy::Omit => {}
            }
            Ok(wire)
        }
        plakous_model::ProjectedToolPresentationV2::CustomUtf8 { format, .. } => {
            if policy.custom_tools == CustomToolPolicy::Reject {
                return Err(ResponsesRequestOptionError::CustomTools.into());
            }
            Ok(json!({
                "type": "custom",
                "name": tool.exposed_name().as_str(),
                "description": tool.description(),
                "format": {
                    "type": "grammar",
                    "syntax": match format.syntax() {
                        plakous_model::ProjectedCustomToolGrammarSyntaxV2::Lark => "lark",
                    },
                    "definition": format.definition(),
                }
            }))
        }
    }
}

pub(super) fn lower_tool_definitions(
    tools: &[ConfiguredTool],
    projection: Option<&CompiledToolProjectionV2>,
    config: ResponsesRequestConfig<'_>,
    custom_tools: &BTreeSet<ToolName>,
) -> Result<Vec<Value>, ResponsesRequestError> {
    if let Some(projection) = projection {
        return projection
            .definitions()
            .iter()
            .map(|entry| {
                let callers = projected_caller_disposition(entry, tools)?;
                let mut wire = lower_projected_tool_definition(entry, config.policy)?;
                lower_allowed_callers(&mut wire, callers)?;
                Ok(wire)
            })
            .collect::<Result<Vec<_>, _>>();
    }
    let mut lowered = Vec::new();
    for tool in tools {
        lowered.push(lower_tool_definition(tool, config.policy, custom_tools)?);
    }
    Ok(lowered)
}

pub(super) fn projected_caller_disposition(
    projected: &ProjectedToolDefinitionV2,
    tools: &[ConfiguredTool],
) -> Result<ToolCallerDisposition, ResponsesRequestError> {
    let mut dispositions = tools
        .iter()
        .filter(|tool| projected.covers_canonical_name(tool.definition().name()))
        .map(ConfiguredTool::caller);
    let first = dispositions
        .next()
        .ok_or(ResponsesRequestOptionError::ProcessProjectionUnavailable)?;
    if dispositions.any(|caller| caller != first) {
        return Err(ResponsesRequestOptionError::ProcessProjectionUnavailable.into());
    }
    Ok(first)
}

pub(super) fn lowered_local_tool_names(
    tools: &[ConfiguredTool],
    projection: Option<&CompiledToolProjectionV2>,
) -> BTreeSet<ToolName> {
    if let Some(projection) = projection {
        return projection
            .definitions()
            .iter()
            .map(|entry| entry.exposed_name().clone())
            .collect();
    }
    tools
        .iter()
        .map(|tool| tool.definition().name().clone())
        .collect()
}

pub(super) fn lower_tool_choice(
    choice: &ToolChoice,
    tools: &[ConfiguredTool],
    custom_tools: &BTreeSet<ToolName>,
) -> Result<Value, ResponsesRequestError> {
    match choice {
        ToolChoice::None => Ok(FunctionToolChoiceWire::None.to_value()),
        ToolChoice::Required => Ok(FunctionToolChoiceWire::Required.to_value()),
        ToolChoice::Specific(name) => {
            let mut selected = tools.iter().filter(|tool| tool.definition().name() == name);
            let tool = selected
                .next()
                .ok_or(ResponsesRequestOptionError::SpecificToolNotAdvertised)?;
            if selected.next().is_some() {
                return Err(ResponsesRequestOptionError::SpecificToolAmbiguous.into());
            }
            if custom_tools.contains(tool.definition().name()) {
                Ok(json!({"type": "custom", "name": name.as_str()}))
            } else {
                Ok(FunctionToolChoiceWire::Function(name.as_str().to_owned()).to_value())
            }
        }
        ToolChoice::Auto => Ok(FunctionToolChoiceWire::Auto.to_value()),
        _ => Err(ResponsesRequestOptionError::UnknownToolChoice.into()),
    }
}

pub(super) fn lower_projected_tool_choice(
    request: &ModelRequest,
    custom_tools: &BTreeSet<ToolName>,
) -> Result<Value, ResponsesRequestError> {
    if let ToolChoice::Specific(name) = request.tool_choice()
        && let Some(projection) = request.tool_projection()
    {
        let mut matches = projection
            .definitions()
            .iter()
            .filter(|entry| entry.covers_canonical_name(name));
        if let Some(entry) = matches.next() {
            if matches.next().is_some() {
                return Err(ResponsesRequestOptionError::SpecificToolAmbiguous.into());
            }
            let kind = if matches!(
                entry.presentation(),
                plakous_model::ProjectedToolPresentationV2::CustomUtf8 { .. }
            ) {
                "custom"
            } else {
                "function"
            };
            return Ok(json!({"type":kind,"name":entry.exposed_name().as_str()}));
        }
    }
    lower_tool_choice(request.tool_choice(), request.tools(), custom_tools)
}
