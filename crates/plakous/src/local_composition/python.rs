//! The Python PTC integration (feature `composition-python`).

#![allow(
    clippy::wildcard_imports,
    reason = "a split of the local composition module that shares its vocabulary"
)]

use super::*;

pub(super) fn attach_python(
    builder: AgentBuilder,
    descriptor: &PythonPtcDescriptorV1,
    worker: PathBuf,
    identity: Vec<u8>,
    attachment: LocalCompositionAttachmentIdentity,
    evidence: ProductExtensionEvidence,
    presentation_kind: plakous_tools::CodingPresentation,
) -> Result<AgentBuilder, AgentError> {
    let nested_names = descriptor
        .nested_tools()
        .iter()
        .map(|tool| tool.canonical_name().clone())
        .collect::<Vec<_>>();
    let python_bindings = descriptor
        .nested_tools()
        .iter()
        .map(|tool| {
            (
                tool.python_name().to_owned(),
                crate::monty_python::PythonToolBinding {
                    tool_name: tool.canonical_name().as_str().to_owned(),
                    input_kind: if tool.string_input() {
                        crate::monty_python::PythonToolInput::String
                    } else {
                        crate::monty_python::PythonToolInput::Dictionary
                    },
                },
            )
        })
        .collect::<BTreeMap<_, _>>();
    let neutral = neutral_python_descriptor(descriptor).map_err(invalid)?;
    let integration: Arc<dyn LocalCompositionIntegration> = Arc::new(PythonIntegration {
        worker,
        identity,
        nested_names: nested_names.into(),
        python_bindings: Arc::new(python_bindings),
        presentation: descriptor.presentation(),
        presentation_kind,
        evidence,
    });
    builder.local_programmatic_composition(LocalCompositionConstruction {
        descriptor: neutral,
        integration,
        attachment,
    })
}

struct PythonIntegration {
    worker: PathBuf,
    identity: Vec<u8>,
    nested_names: Arc<[ToolName]>,
    python_bindings: Arc<BTreeMap<String, crate::monty_python::PythonToolBinding>>,
    presentation: plakous_compose::PythonPtcPresentationV1,
    presentation_kind: plakous_tools::CodingPresentation,
    evidence: ProductExtensionEvidence,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PythonStructuredRunInput {
    pub(super) code: String,
    yield_time_ms: Option<f64>,
    pub(super) max_output_bytes: Option<f64>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct PythonWaitInput {
    cell_id: String,
    yield_time_ms: Option<f64>,
    max_output_bytes: Option<f64>,
    terminate: Option<bool>,
}

pub(super) fn parse_python_wait(
    input: serde_json::Value,
) -> Result<LocalCompositionWaitInput, String> {
    let input: PythonWaitInput =
        serde_json::from_value(input).map_err(|_| "invalid Python wait input".to_owned())?;
    if input.cell_id.is_empty() {
        return Err("Python wait cell_id must not be empty".into());
    }
    let yield_time_ms = input
        .yield_time_ms
        .map_or(Ok(PYTHON_DEFAULT_YIELD_TIME_MS), |value| {
            whole_u64(value, "yield_time_ms")
        })?
        .min(600_000);
    let max_output_bytes = input
        .max_output_bytes
        .map_or(Ok(PythonPtcLimitsV1::EXACT.final_result_bytes), |value| {
            whole_u64(value, "max_output_bytes")
        })?;
    if max_output_bytes == 0 || max_output_bytes > PythonPtcLimitsV1::EXACT.final_result_bytes {
        return Err("Python wait output byte bound exceeded".into());
    }
    Ok(LocalCompositionWaitInput {
        cell_id: input.cell_id,
        yield_time_ms,
        max_output_tokens: usize::try_from(max_output_bytes)
            .map_err(|_| "Python output bound exceeds usize".to_owned())?,
        terminate: input.terminate.unwrap_or(false),
    })
}

impl LocalCompositionIntegration for PythonIntegration {
    fn runtime_contract(&self) -> LocalCompositionRuntimeContract {
        LocalCompositionRuntimeContract::PYTHON
    }
    fn lifecycle(&self) -> LocalCompositionLifecycle {
        LocalCompositionLifecycle::AgentIncarnationPersistent
    }
    fn result_projection(&self) -> LocalCompositionResultProjection {
        LocalCompositionResultProjection::BytesPythonPtcV1
    }
    fn environment_identity(&self) -> Vec<u8> {
        let mut value = self.identity.clone();
        value.extend_from_slice(&self.evidence.identity().identity_bytes());
        value
    }
    fn parse_execute(
        &self,
        input: serde_json::Value,
    ) -> Result<LocalCompositionExecuteInput, String> {
        let (source, max_output_bytes, yield_time_ms) = match self.presentation {
            plakous_compose::PythonPtcPresentationV1::StructuredFunctions => {
                let value: PythonStructuredRunInput = serde_json::from_value(input)
                    .map_err(|_| "invalid structured Python run input".to_owned())?;
                (
                    value.code,
                    value
                        .max_output_bytes
                        .map_or(Ok(PythonPtcLimitsV1::EXACT.final_result_bytes), |value| {
                            whole_u64(value, "max_output_bytes")
                        })?,
                    value
                        .yield_time_ms
                        .map_or(Ok(PYTHON_DEFAULT_YIELD_TIME_MS), |value| {
                            whole_u64(value, "yield_time_ms")
                        })?,
                )
            }
            plakous_compose::PythonPtcPresentationV1::CustomRunFunctionWait => {
                let source = input
                    .as_str()
                    .ok_or_else(|| "custom Python run input must be raw UTF-8 text".to_owned())?
                    .to_owned();
                (
                    source,
                    PythonPtcLimitsV1::EXACT.final_result_bytes,
                    PYTHON_DEFAULT_YIELD_TIME_MS,
                )
            }
        };
        if source.is_empty() || source.len() > PYTHON_SOURCE_MAX_BYTES {
            return Err("Python source must be non-empty and within 256 KiB".into());
        }
        if yield_time_ms > PYTHON_DEFAULT_YIELD_TIME_MS
            || max_output_bytes == 0
            || max_output_bytes > PythonPtcLimitsV1::EXACT.final_result_bytes
        {
            return Err("Python run input exceeds its exact byte bound".into());
        }
        Ok(LocalCompositionExecuteInput {
            source,
            yield_time_ms,
            max_output_tokens: usize::try_from(max_output_bytes)
                .map_err(|_| "Python output byte bound exceeds usize".to_owned())?,
        })
    }
    fn parse_wait(&self, input: serde_json::Value) -> Result<LocalCompositionWaitInput, String> {
        parse_python_wait(input)
    }
    fn create_session(
        &self,
        _generation: u64,
        _max_yield_time_ms: u64,
        _max_output_tokens: usize,
    ) -> LocalCompositionFuture<'_, LocalCompositionSessionBundle> {
        Box::pin(async move {
            crate::monty_python::session_bundle(&self.worker, (*self.python_bindings).clone()).await
        })
    }
    fn shutdown(
        &self,
    ) -> Pin<Box<dyn Future<Output = LocalCompositionShutdownResult> + Send + '_>> {
        Box::pin(async { LocalCompositionShutdownResult::Joined { child_exit: None } })
    }
    fn rebuild_descriptor(
        &self,
        job_config: plakous_tools::JobConfig,
        registry: FrozenToolRegistry,
        target: ProviderModelProjectionIdentityV2,
    ) -> Result<LocalCompositionDescriptor, String> {
        PythonPtcV1::compile_for_kind(
            job_config,
            registry,
            target,
            self.presentation,
            self.presentation_kind,
            self.nested_names.iter().cloned(),
        )
        .map_err(|error| error.to_string())
        .and_then(|value| neutral_python_descriptor(&value))
    }
    fn validate_configuration(
        &self,
        job_config: plakous_tools::JobConfig,
        descriptor: &LocalCompositionDescriptor,
        projection: &ToolProjectionMap,
        registry: FrozenToolRegistry,
        configuration: &CompiledProviderConfiguration,
    ) -> Result<(), String> {
        validate_python_configuration(
            job_config,
            descriptor,
            projection,
            registry,
            configuration,
            &self.nested_names,
            self.presentation,
            self.presentation_kind,
            &self.evidence,
        )
    }
    fn truncate_items(
        &self,
        items: Vec<LocalCompositionContentItem>,
        maximum: usize,
    ) -> Vec<LocalCompositionContentItem> {
        truncate_python_items(items, maximum)
    }
    fn max_yield_time_ms(&self) -> u64 {
        PYTHON_DEFAULT_YIELD_TIME_MS
    }
    fn max_output_tokens(&self) -> usize {
        usize::try_from(PythonPtcLimitsV1::EXACT.final_result_bytes).unwrap_or(usize::MAX)
    }
    fn max_outstanding_nested_calls(&self) -> usize {
        PythonPtcLimitsV1::EXACT.concurrent_nested_calls as usize
    }
}

pub(super) fn truncate_python_items(
    items: Vec<LocalCompositionContentItem>,
    maximum: usize,
) -> Vec<LocalCompositionContentItem> {
    let mut remaining = maximum;
    let mut output = Vec::new();
    for item in items {
        if remaining == 0 {
            break;
        }
        if let LocalCompositionContentItem::Text(mut text) = item {
            if text.len() > remaining {
                let mut boundary = remaining;
                while boundary > 0 && !text.is_char_boundary(boundary) {
                    boundary -= 1;
                }
                text.truncate(boundary);
            }
            remaining = remaining.saturating_sub(text.len());
            if !text.is_empty() {
                output.push(LocalCompositionContentItem::Text(text));
            }
        }
    }
    output
}

pub(super) fn neutral_python_descriptor(
    exact: &PythonPtcDescriptorV1,
) -> Result<LocalCompositionDescriptor, String> {
    let nested_tools = exact
        .nested_tools()
        .iter()
        .map(|tool| LocalCompositionToolDefinition {
            name: tool.python_name().to_owned(),
            description: tool.description().to_owned(),
            input_schema: tool.composition_input_schema_value(),
        })
        .collect::<Vec<_>>();
    Ok(LocalCompositionDescriptor {
        outer_projection: exact.outer_projection().clone(),
        nested_projection: exact.nested_projection().clone(),
        nested_tools: nested_tools.into(),
        identity: exact.identity(),
        execute_name: PYTHON_PTC_RUN_NAME_V1.into(),
        wait_name: PYTHON_PTC_WAIT_NAME_V1.into(),
        marker_executor: ToolExecutorId::new(PYTHON_PTC_DRIVER_MARKER_EXECUTOR_ID_V1)
            .map_err(|error| error.to_string())?,
    })
}
