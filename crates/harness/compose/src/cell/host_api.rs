use plakous_model::{Digest, ProjectedToolPresentationV2, ToolName};
use plakous_tools::{PortableToolSchemaV2, ToolProjectionMap, ToolRuntimeError};
use serde::Serialize;

use super::compile::registration_error;
use super::presentation::{
    CatalogStyle, CodexCodeModeNestedToolV1, ProductJavaScriptNestedToolV1, PythonPtcNestedToolV1,
};
use super::render_py::normalize_python_identifier;

/// The ordered namespaces and function contracts available inside a cell.
#[derive(Clone, Debug, Serialize)]
pub struct HostApi {
    pub(super) namespaces: Vec<ApiNamespace>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) sdk: Option<super::sdk_api::SdkApi>,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct ApiNamespace {
    pub name: String,
    pub functions: Vec<ApiFunction>,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct ApiParameter {
    pub name: String,
    pub input: ApiInput,
}

#[derive(Clone, Debug, Serialize)]
pub(super) enum ApiInput {
    String,
    Structured(Option<PortableToolSchemaV2>),
}

#[derive(Clone, Copy, Debug, Serialize)]
pub(super) enum ApiReturn {
    Text,
    Object,
    Unknown,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct ApiFunction {
    pub exposed_name: ToolName,
    pub python_name: String,
    pub description: String,
    pub parameters: Vec<ApiParameter>,
    pub returns: ApiReturn,
    pub projection_identity: Option<Digest>,
}

impl ApiFunction {
    pub fn string_input(&self) -> bool {
        matches!(self.parameters[0].input, ApiInput::String)
    }

    pub fn input_schema(&self) -> Option<&PortableToolSchemaV2> {
        match &self.parameters[0].input {
            ApiInput::String => None,
            ApiInput::Structured(schema) => schema.as_ref(),
        }
    }

    pub fn returns_object(&self) -> bool {
        matches!(self.returns, ApiReturn::Object)
    }

    pub fn projection_identity(&self) -> Result<Digest, ToolRuntimeError> {
        self.projection_identity
            .ok_or_else(|| registration_error("nested host API projection identity is missing"))
    }
}

impl HostApi {
    /// Generates the SDK typing module, when this is an SDK host catalog.
    #[must_use]
    pub fn python_sdk_stubs(&self) -> Option<String> {
        self.sdk.as_ref().map(super::sdk_api::SdkApi::stubs)
    }

    /// Validates a typed SDK call against this exact host catalog.
    ///
    /// # Errors
    /// Rejects non-SDK catalogs, unknown functions, argument keys or shapes.
    pub fn validate_python_sdk_call(
        &self,
        name: &str,
        input: &serde_json::Value,
    ) -> Result<(), String> {
        self.sdk
            .as_ref()
            .ok_or_else(|| "host catalog is not a Python SDK".to_owned())?
            .validate(name, input)
    }
    #[cfg(test)]
    pub(super) fn codex_catalog(tools: &[CodexCodeModeNestedToolV1]) -> Self {
        Self {
            sdk: None,
            namespaces: vec![ApiNamespace {
                name: "tools".into(),
                functions: tools
                    .iter()
                    .map(|tool| ApiFunction {
                        exposed_name: tool.exposed_name.clone(),
                        python_name: normalize_python_identifier(tool.exposed_name.as_str()),
                        description: tool.description.clone(),
                        parameters: vec![ApiParameter {
                            name: "arguments".into(),
                            input: tool
                                .input_schema
                                .clone()
                                .map_or(ApiInput::String, |schema| {
                                    ApiInput::Structured(Some(schema))
                                }),
                        }],
                        returns: if matches!(
                            tool.exposed_name.as_str(),
                            "exec_command" | "write_stdin"
                        ) {
                            ApiReturn::Object
                        } else {
                            ApiReturn::Unknown
                        },
                        projection_identity: Some(tool.projection_identity),
                    })
                    .collect(),
            }],
        }
    }

    pub(super) fn from_projection(
        projection: &ToolProjectionMap,
        names: &[ToolName],
        style: CatalogStyle,
    ) -> Result<Self, ToolRuntimeError> {
        let codex = matches!(style, CatalogStyle::CodexExact | CatalogStyle::CodexMinimal);
        let view = codex
            .then(|| projection.compile_request_view_v2())
            .transpose()?;
        let functions = names
            .iter()
            .map(|name| {
                let entry = projection
                    .by_exposed(name)
                    .ok_or_else(|| registration_error("nested projection member is absent"))?;
                let (description, presentation, schema, identity) = if let Some(view) = &view {
                    let definition = view
                        .definitions()
                        .iter()
                        .find(|definition| definition.exposed_name() == name)
                        .ok_or_else(|| {
                            registration_error("exact Code Mode nested tool is absent")
                        })?;
                    (
                        definition.description(),
                        definition.presentation(),
                        definition.input_schema(),
                        definition.source_identity(),
                    )
                } else {
                    (
                        entry.description(),
                        entry.presentation(),
                        entry.input_schema(),
                        entry.identity(),
                    )
                };
                let input = match presentation {
                    ProjectedToolPresentationV2::CustomUtf8 { .. } => ApiInput::String,
                    ProjectedToolPresentationV2::StructuredFunction { .. } => {
                        ApiInput::Structured(Some(
                            schema
                                .ok_or_else(|| {
                                    registration_error("nested projection is not structured")
                                })?
                                .clone(),
                        ))
                    }
                };
                let returns = if codex {
                    if matches!(name.as_str(), "exec_command" | "write_stdin") {
                        ApiReturn::Object
                    } else {
                        ApiReturn::Unknown
                    }
                } else if entry.local_programmatic_returns_object() {
                    ApiReturn::Object
                } else {
                    ApiReturn::Text
                };
                Ok(ApiFunction {
                    exposed_name: name.clone(),
                    python_name: normalize_python_identifier(name.as_str()),
                    description: description.to_owned(),
                    parameters: vec![ApiParameter {
                        name: "arguments".to_owned(),
                        input,
                    }],
                    returns,
                    projection_identity: Some(identity),
                })
            })
            .collect::<Result<_, ToolRuntimeError>>()?;
        Ok(Self {
            sdk: (style == CatalogStyle::PythonSdk).then(super::sdk_api::SdkApi::core),
            namespaces: vec![ApiNamespace {
                name: "tools".to_owned(),
                functions,
            }],
        })
    }

    pub(super) fn javascript_tools(
        &self,
    ) -> Result<Vec<ProductJavaScriptNestedToolV1>, ToolRuntimeError> {
        self.namespaces
            .iter()
            .flat_map(|namespace| &namespace.functions)
            .map(|tool| {
                Ok(ProductJavaScriptNestedToolV1 {
                    exposed_name: tool.exposed_name.clone(),
                    description: tool.description.clone(),
                    input_schema: tool.input_schema().cloned(),
                    string_input: tool.string_input(),
                    projection_identity: tool.projection_identity()?,
                })
            })
            .collect()
    }

    pub(super) fn python_tools(&self) -> Result<Vec<PythonPtcNestedToolV1>, ToolRuntimeError> {
        self.namespaces
            .iter()
            .flat_map(|namespace| &namespace.functions)
            .map(|tool| {
                Ok(PythonPtcNestedToolV1 {
                    canonical_name: tool.exposed_name.clone(),
                    python_name: tool.python_name.clone(),
                    description: tool.description.clone(),
                    input_schema: tool.input_schema().cloned(),
                    string_input: tool.string_input(),
                    projection_identity: tool.projection_identity()?,
                })
            })
            .collect()
    }

    pub(super) fn codex_tools(&self) -> Result<Vec<CodexCodeModeNestedToolV1>, ToolRuntimeError> {
        self.namespaces
            .iter()
            .flat_map(|namespace| &namespace.functions)
            .map(|tool| {
                Ok(CodexCodeModeNestedToolV1 {
                    exposed_name: tool.exposed_name.clone(),
                    description: tool.description.clone(),
                    input_schema: tool.input_schema().cloned(),
                    string_input: tool.string_input(),
                    projection_identity: tool.projection_identity()?,
                })
            })
            .collect()
    }
}
