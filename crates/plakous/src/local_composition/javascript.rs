//! Product JavaScript attachment and the JavaScript descriptor conversions.

#![allow(
    clippy::wildcard_imports,
    reason = "a split of the local composition module that shares its vocabulary"
)]

use super::*;

pub(super) fn attach_product_javascript(
    builder: AgentBuilder,
    descriptor: &ProductJavaScriptCompositionDescriptorV1,
    host: PathBuf,
    identity: Vec<u8>,
    attachment: LocalCompositionAttachmentIdentity,
    evidence: ProductExtensionEvidence,
    presentation_kind: plakous_tools::CodingPresentation,
) -> Result<AgentBuilder, AgentError> {
    let (_, _, _, _, limits, _) = evidence.extension_facts();
    let max_output_tokens = usize::try_from(limits.max_output_tokens).map_err(|_| {
        AgentError::new(
            AgentErrorKind::InvalidInput,
            "product extension output limit exceeds the host integer boundary",
        )
    })?;
    let nested_names = descriptor
        .nested_tools()
        .iter()
        .map(|tool| tool.exposed_name().clone())
        .collect::<Vec<_>>();
    let neutral = neutral_product_descriptor(descriptor)
        .map_err(|error| AgentError::new(AgentErrorKind::InvalidInput, error))?;
    let integration: Arc<dyn LocalCompositionIntegration> = Arc::new(CellIntegration {
        worker: host,
        identity,
        presentation: Box::new(JavaScriptPresentation::Product(Box::new(
            ProductJavaScript {
                nested_names: nested_names.into(),
                presentation_kind,
                evidence,
                limits,
                max_output_tokens,
            },
        ))),
    });
    builder.local_programmatic_composition(LocalCompositionConstruction {
        descriptor: neutral,
        integration,
        attachment,
    })
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WaitWireInput {
    pub(crate) cell_id: String,
    pub(crate) yield_time_ms: Option<f64>,
    pub(crate) max_tokens: Option<f64>,
    pub(crate) terminate: Option<bool>,
}

pub(crate) fn whole_u64(value: f64, field: &str) -> Result<u64, String> {
    if !value.is_finite() || value < 0.0 || value.fract() != 0.0 || value > 9_007_199_254_740_991.0
    {
        return Err(format!(
            "Code Mode wait {field} must be a non-negative whole number"
        ));
    }
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "range checked above"
    )]
    Ok(value as u64)
}

pub(crate) fn neutral_descriptor(
    exact: &CodexNativeLocalCodeModeDescriptorV1,
) -> Result<LocalCompositionDescriptor, String> {
    let nested_tools = exact
        .nested_tools()
        .iter()
        .map(|tool| LocalCompositionToolDefinition {
            name: tool.exposed_name().as_str().to_owned(),
            description: tool.description().to_owned(),
            input_schema: tool.composition_input_schema_value(),
        })
        .collect::<Vec<_>>();
    Ok(LocalCompositionDescriptor {
        outer_projection: exact.outer_projection().clone(),
        nested_projection: exact.nested_projection().clone(),
        nested_tools: nested_tools.into(),
        identity: exact.identity(),
        execute_name: "exec".into(),
        wait_name: "wait".into(),
        marker_executor: ToolExecutorId::new("plakous.codex-code-mode.driver-marker.v1")
            .map_err(|error| error.to_string())?,
    })
}

pub(crate) fn neutral_minimal_descriptor(
    exact: &CodexMinimalLocalCodeModeDescriptorV1,
) -> Result<LocalCompositionDescriptor, String> {
    let nested_tools = exact
        .nested_tools()
        .iter()
        .map(|tool| LocalCompositionToolDefinition {
            name: tool.exposed_name().as_str().to_owned(),
            description: tool.description().to_owned(),
            input_schema: tool.composition_input_schema_value(),
        })
        .collect::<Vec<_>>();
    Ok(LocalCompositionDescriptor {
        outer_projection: exact.outer_projection().clone(),
        nested_projection: exact.nested_projection().clone(),
        nested_tools: nested_tools.into(),
        identity: exact.identity(),
        execute_name: "exec".into(),
        wait_name: "wait".into(),
        marker_executor: ToolExecutorId::new(
            plakous_tools::CODEX_NATIVE_CODE_MODE_DRIVER_MARKER_EXECUTOR_ID_V1,
        )
        .map_err(|error| error.to_string())?,
    })
}

pub(super) fn neutral_product_descriptor(
    exact: &ProductJavaScriptCompositionDescriptorV1,
) -> Result<LocalCompositionDescriptor, String> {
    let nested_tools = exact
        .nested_tools()
        .iter()
        .map(|tool| LocalCompositionToolDefinition {
            name: tool.exposed_name().as_str().to_owned(),
            description: tool.description().to_owned(),
            input_schema: tool.composition_input_schema_value(),
        })
        .collect::<Vec<_>>();
    Ok(LocalCompositionDescriptor {
        outer_projection: exact.outer_projection().clone(),
        nested_projection: exact.nested_projection().clone(),
        nested_tools: nested_tools.into(),
        identity: exact.identity(),
        execute_name: PRODUCT_JAVASCRIPT_RUN_NAME_V1.into(),
        wait_name: PRODUCT_JAVASCRIPT_WAIT_NAME_V1.into(),
        marker_executor: ToolExecutorId::new(PRODUCT_JAVASCRIPT_DRIVER_MARKER_EXECUTOR_ID_V1)
            .map_err(|error| error.to_string())?,
    })
}

pub(crate) fn from_native_item(value: native::ContentItem) -> LocalCompositionContentItem {
    match value {
        native::ContentItem::InputText { text } => LocalCompositionContentItem::Text(text),
        native::ContentItem::InputImage { image_url, detail } => {
            LocalCompositionContentItem::Image {
                url: image_url,
                detail: detail.map(from_native_detail),
            }
        }
        native::ContentItem::InputAudio { audio_url } => {
            LocalCompositionContentItem::Audio(audio_url)
        }
    }
}
fn from_native_detail(value: native::ImageDetail) -> LocalCompositionImageDetail {
    match value {
        native::ImageDetail::Auto => LocalCompositionImageDetail::Auto,
        native::ImageDetail::Low => LocalCompositionImageDetail::Low,
        native::ImageDetail::High => LocalCompositionImageDetail::High,
        native::ImageDetail::Original => LocalCompositionImageDetail::Original,
    }
}
pub(crate) fn to_native_item(value: LocalCompositionContentItem) -> native::ContentItem {
    match value {
        LocalCompositionContentItem::Text(text) => native::ContentItem::InputText { text },
        LocalCompositionContentItem::Image { url, detail } => native::ContentItem::InputImage {
            image_url: url,
            detail: detail.map(|value| match value {
                LocalCompositionImageDetail::Auto => native::ImageDetail::Auto,
                LocalCompositionImageDetail::Low => native::ImageDetail::Low,
                LocalCompositionImageDetail::High => native::ImageDetail::High,
                LocalCompositionImageDetail::Original => native::ImageDetail::Original,
            }),
        },
        LocalCompositionContentItem::Audio(audio_url) => {
            native::ContentItem::InputAudio { audio_url }
        }
    }
}
