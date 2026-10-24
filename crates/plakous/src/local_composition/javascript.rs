//! The product JavaScript integration and the Codex descriptor conversions.

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
    let integration: Arc<dyn LocalCompositionIntegration> =
        Arc::new(ProductJavaScriptIntegration {
            sidecar: host,
            identity,
            nested_names: nested_names.into(),
            presentation_kind,
            evidence,
            limits,
            max_output_tokens,
        });
    builder.local_programmatic_composition(LocalCompositionConstruction {
        descriptor: neutral,
        integration,
        attachment,
    })
}

struct ProductJavaScriptIntegration {
    sidecar: PathBuf,
    identity: Vec<u8>,
    nested_names: Arc<[ToolName]>,
    presentation_kind: plakous_tools::CodingPresentation,
    evidence: ProductExtensionEvidence,
    limits: ProductExtensionLimits,
    max_output_tokens: usize,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ProductRunWireInput {
    source: String,
    yield_time_ms: Option<u64>,
    max_output_tokens: Option<u64>,
}

impl LocalCompositionIntegration for ProductJavaScriptIntegration {
    fn runtime_contract(&self) -> LocalCompositionRuntimeContract {
        LocalCompositionRuntimeContract::JAVASCRIPT
    }
    fn lifecycle(&self) -> LocalCompositionLifecycle {
        LocalCompositionLifecycle::RootTurnJoined
    }

    fn result_projection(&self) -> LocalCompositionResultProjection {
        LocalCompositionResultProjection::TokensScriptV1
    }

    fn environment_identity(&self) -> Vec<u8> {
        let mut identity = self.identity.clone();
        identity.extend_from_slice(&self.evidence.identity().identity_bytes());
        identity
    }

    fn parse_execute(
        &self,
        input: serde_json::Value,
    ) -> Result<LocalCompositionExecuteInput, String> {
        let input: ProductRunWireInput = serde_json::from_value(input)
            .map_err(|_| "invalid product JavaScript run input".to_owned())?;
        if input.source.is_empty()
            || u64::try_from(input.source.len())
                .map_or(true, |length| length > self.limits.max_source_bytes)
        {
            return Err(
                "product JavaScript source must be non-empty and within its UTF-8 byte limit"
                    .to_owned(),
            );
        }
        let yield_time_ms = input
            .yield_time_ms
            .unwrap_or(PRODUCT_JAVASCRIPT_DEFAULT_YIELD_TIME_MS);
        let max_output_tokens = input
            .max_output_tokens
            .unwrap_or(PRODUCT_JAVASCRIPT_DEFAULT_OUTPUT_TOKENS);
        if yield_time_ms > self.limits.max_yield_time_ms
            || max_output_tokens == 0
            || max_output_tokens > self.limits.max_output_tokens
        {
            return Err(
                "product JavaScript run input exceeds its evidence-bound limits".to_owned(),
            );
        }
        Ok(LocalCompositionExecuteInput {
            source: input.source,
            yield_time_ms,
            max_output_tokens: usize::try_from(max_output_tokens)
                .map_err(|_| "product JavaScript output limit exceeds usize".to_owned())?,
        })
    }

    fn parse_wait(&self, input: serde_json::Value) -> Result<LocalCompositionWaitInput, String> {
        let input: WaitWireInput = serde_json::from_value(input)
            .map_err(|_| "invalid product JavaScript wait input".to_owned())?;
        if input.cell_id.is_empty() {
            return Err("product JavaScript wait cell_id must not be empty".to_owned());
        }
        let yield_time_ms = input
            .yield_time_ms
            .map_or(Ok(PRODUCT_JAVASCRIPT_DEFAULT_YIELD_TIME_MS), |value| {
                whole_u64(value, "yield-time_ms")
            })?
            .min(600_000);
        let default_output_tokens = usize::try_from(PRODUCT_JAVASCRIPT_DEFAULT_OUTPUT_TOKENS)
            .map_err(|_| "product JavaScript default output limit exceeds usize".to_owned())?;
        let max_output_tokens = input
            .max_tokens
            .map_or(Ok(default_output_tokens), |value| {
                whole_u64(value, "max_tokens").and_then(|value| {
                    usize::try_from(value)
                        .map_err(|_| "product JavaScript wait max_tokens exceeds usize".to_owned())
                })
            })?;
        if max_output_tokens == 0 || max_output_tokens > self.max_output_tokens {
            return Err(
                "product JavaScript wait input exceeds its evidence-bound limits".to_owned(),
            );
        }
        Ok(LocalCompositionWaitInput {
            cell_id: input.cell_id,
            yield_time_ms,
            max_output_tokens,
            terminate: input.terminate.unwrap_or(false),
        })
    }

    fn create_session(
        &self,
        _generation: u64,
        _max_yield_time_ms: u64,
        _max_output_tokens: usize,
    ) -> LocalCompositionFuture<'_, LocalCompositionSessionBundle> {
        let sidecar = self.sidecar.clone();
        let identity = self.identity.clone();
        Box::pin(async move {
            verify_sidecar_before_spawn(&sidecar, &identity)?;
            let host = plakous_js_eval::SidecarHost::spawn(&sidecar)?;
            Ok(crate::js_code_mode::session_bundle_from_engine(Arc::new(
                host,
            )))
        })
    }

    fn shutdown(
        &self,
    ) -> Pin<Box<dyn Future<Output = LocalCompositionShutdownResult> + Send + '_>> {
        Box::pin(async move { LocalCompositionShutdownResult::Joined { child_exit: None } })
    }

    fn rebuild_descriptor(
        &self,
        job_config: plakous_tools::JobConfig,
        registry: FrozenToolRegistry,
        target: ProviderModelProjectionIdentityV2,
    ) -> Result<LocalCompositionDescriptor, String> {
        ProductJavaScriptCompositionV1::compile_for_kind(
            job_config,
            registry,
            target,
            self.presentation_kind,
            self.nested_names.iter().cloned(),
        )
        .map_err(|error| error.to_string())
        .and_then(|descriptor| neutral_product_descriptor(&descriptor))
    }

    fn validate_configuration(
        &self,
        job_config: plakous_tools::JobConfig,
        descriptor: &LocalCompositionDescriptor,
        projection: &ToolProjectionMap,
        registry: FrozenToolRegistry,
        configuration: &CompiledProviderConfiguration,
    ) -> Result<(), String> {
        validate_product_configuration(
            job_config,
            descriptor,
            projection,
            registry,
            configuration,
            &self.nested_names,
            self.presentation_kind,
            &self.evidence,
        )
    }

    fn truncate_items(
        &self,
        items: Vec<LocalCompositionContentItem>,
        max_output_tokens: usize,
    ) -> Vec<LocalCompositionContentItem> {
        native::truncate_content_items(
            items.into_iter().map(to_native_item).collect(),
            max_output_tokens,
        )
        .into_iter()
        .map(from_native_item)
        .collect()
    }

    fn max_yield_time_ms(&self) -> u64 {
        self.limits.max_yield_time_ms
    }

    fn max_output_tokens(&self) -> usize {
        self.max_output_tokens
    }
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
