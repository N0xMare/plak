//! The one integration for every cell presentation. A presentation decides
//! what the model sees (parsing, limits, descriptor, evidence) and which
//! worker runs it; the integration, the cell client and the session seam are
//! the same for all of them: Codex Code Mode (exact and minimal) and product
//! JavaScript on the `QuickJS` worker, and Python PTC on the Monty worker.

#![allow(
    clippy::wildcard_imports,
    reason = "a split of the local composition module that shares its vocabulary"
)]

use super::*;

use super::session::{Quickjs, SessionEngine, session_bundle};
use plakous_cell::{CellClient, EngineConfig, QUICKJS_WORKER, WorkerIdentity};
use plakous_compose::{
    CodexMinimalLocalCodeModeV1, CodexNativeLocalCodeModeV1,
    validate_codex_minimal_local_code_mode_candidate_v1,
    validate_codex_native_local_code_mode_candidate_v1,
};

pub(super) struct CellIntegration {
    pub(super) worker: PathBuf,
    pub(super) identity: Vec<u8>,
    pub(super) presentation: Box<dyn Presentation>,
}

/// One cell presentation: its model-visible contract and its worker.
pub(super) trait Presentation: Send + Sync {
    fn foreground_budget(&self) -> Option<std::time::Duration> {
        None
    }
    fn consumes_values(&self) -> bool {
        false
    }
    fn runtime_contract(&self) -> LocalCompositionRuntimeContract;
    fn lifecycle(&self) -> LocalCompositionLifecycle;
    fn result_projection(&self) -> LocalCompositionResultProjection;
    /// Evidence whose identity the environment identity carries.
    fn evidence(&self) -> Option<&ProductExtensionEvidence>;
    fn parse_execute(
        &self,
        input: serde_json::Value,
    ) -> Result<LocalCompositionExecuteInput, LocalCompositionError>;
    fn parse_wait(
        &self,
        input: serde_json::Value,
    ) -> Result<LocalCompositionWaitInput, LocalCompositionError>;
    fn rebuild_descriptor(
        &self,
        job_config: plakous_tools::JobConfig,
        registry: FrozenToolRegistry,
        target: ProviderModelProjectionIdentityV2,
    ) -> Result<LocalCompositionDescriptor, LocalCompositionError>;
    fn validate_configuration(
        &self,
        job_config: plakous_tools::JobConfig,
        descriptor: &LocalCompositionDescriptor,
        projection: &ToolProjectionMap,
        registry: FrozenToolRegistry,
        configuration: &CompiledProviderConfiguration,
    ) -> Result<(), LocalCompositionError>;
    fn truncate_items(
        &self,
        items: Vec<LocalCompositionContentItem>,
        max_output_tokens: usize,
    ) -> Vec<LocalCompositionContentItem>;
    fn max_yield_time_ms(&self) -> u64;
    fn max_output_tokens(&self) -> usize;
    fn max_outstanding_nested_calls(&self) -> usize {
        16
    }
    /// The worker build and limits this presentation runs on.
    fn worker(&self) -> (WorkerIdentity, EngineConfig);
    /// Refuses a worker whose bytes changed since attachment.
    fn verify_worker(&self, path: &Path, identity: &[u8]) -> Result<(), LocalCompositionError>;
    /// The engine state of a fresh session.
    fn session_engine(&self) -> Box<dyn SessionEngine>;
}

impl LocalCompositionIntegration for CellIntegration {
    fn foreground_budget(&self) -> Option<std::time::Duration> {
        self.presentation.foreground_budget()
    }
    fn consumes_values(&self) -> bool {
        self.presentation.consumes_values()
    }
    fn runtime_contract(&self) -> LocalCompositionRuntimeContract {
        self.presentation.runtime_contract()
    }

    fn lifecycle(&self) -> LocalCompositionLifecycle {
        self.presentation.lifecycle()
    }

    fn result_projection(&self) -> LocalCompositionResultProjection {
        self.presentation.result_projection()
    }

    fn environment_identity(&self) -> Vec<u8> {
        let mut identity = self.identity.clone();
        if let Some(evidence) = self.presentation.evidence() {
            identity.extend_from_slice(&evidence.identity().identity_bytes());
        }
        identity
    }

    fn parse_execute(
        &self,
        input: serde_json::Value,
    ) -> Result<LocalCompositionExecuteInput, LocalCompositionError> {
        self.presentation.parse_execute(input)
    }

    fn parse_wait(
        &self,
        input: serde_json::Value,
    ) -> Result<LocalCompositionWaitInput, LocalCompositionError> {
        self.presentation.parse_wait(input)
    }

    fn create_session(
        &self,
        _generation: u64,
        _max_yield_time_ms: u64,
        _max_output_tokens: usize,
    ) -> LocalCompositionFuture<'_, LocalCompositionSessionBundle> {
        Box::pin(async move {
            self.presentation
                .verify_worker(&self.worker, &self.identity)?;
            let (expected, config) = self.presentation.worker();
            let client = CellClient::spawn(&self.worker, expected, config)
                .await
                .map_err(LocalCompositionError::from_source)?;
            Ok(session_bundle(
                Arc::new(client),
                self.presentation.session_engine(),
            ))
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
    ) -> Result<LocalCompositionDescriptor, LocalCompositionError> {
        self.presentation
            .rebuild_descriptor(job_config, registry, target)
    }

    fn validate_configuration(
        &self,
        job_config: plakous_tools::JobConfig,
        descriptor: &LocalCompositionDescriptor,
        projection: &ToolProjectionMap,
        registry: FrozenToolRegistry,
        configuration: &CompiledProviderConfiguration,
    ) -> Result<(), LocalCompositionError> {
        self.presentation.validate_configuration(
            job_config,
            descriptor,
            projection,
            registry,
            configuration,
        )
    }

    fn truncate_items(
        &self,
        items: Vec<LocalCompositionContentItem>,
        max_output_tokens: usize,
    ) -> Vec<LocalCompositionContentItem> {
        self.presentation.truncate_items(items, max_output_tokens)
    }

    fn max_yield_time_ms(&self) -> u64 {
        self.presentation.max_yield_time_ms()
    }

    fn max_output_tokens(&self) -> usize {
        self.presentation.max_output_tokens()
    }

    fn max_outstanding_nested_calls(&self) -> usize {
        self.presentation.max_outstanding_nested_calls()
    }
}

/// The JavaScript presentations on the `QuickJS` worker.
pub(super) enum JavaScriptPresentation {
    CodexExact,
    CodexMinimal { nested_catalog: Vec<ToolName> },
    Product(Box<ProductJavaScript>),
}

pub(super) struct ProductJavaScript {
    pub(super) nested_names: Arc<[ToolName]>,
    pub(super) presentation_kind: plakous_tools::CodingPresentation,
    pub(super) evidence: ProductExtensionEvidence,
    pub(super) limits: ProductExtensionLimits,
    pub(super) max_output_tokens: usize,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ProductRunWireInput {
    source: String,
    yield_time_ms: Option<u64>,
    max_output_tokens: Option<u64>,
}

impl Presentation for JavaScriptPresentation {
    fn runtime_contract(&self) -> LocalCompositionRuntimeContract {
        LocalCompositionRuntimeContract::JAVASCRIPT
    }

    fn lifecycle(&self) -> LocalCompositionLifecycle {
        LocalCompositionLifecycle::RootTurnJoined
    }

    fn result_projection(&self) -> LocalCompositionResultProjection {
        LocalCompositionResultProjection::TokensScriptV1
    }

    fn evidence(&self) -> Option<&ProductExtensionEvidence> {
        match self {
            Self::Product(product) => Some(&product.evidence),
            Self::CodexExact | Self::CodexMinimal { .. } => None,
        }
    }

    fn parse_execute(
        &self,
        input: serde_json::Value,
    ) -> Result<LocalCompositionExecuteInput, LocalCompositionError> {
        match self {
            JavaScriptPresentation::CodexExact | JavaScriptPresentation::CodexMinimal { .. } => {
                let serde_json::Value::String(raw) = input else {
                    return Err("Code Mode exec input is not a custom UTF-8 string".into());
                };
                let parsed = native::parse_exec_source(&raw)?;
                Ok(LocalCompositionExecuteInput {
                    source: parsed.code,
                    yield_time_ms: parsed.yield_time_ms.unwrap_or(10_000),
                    max_output_tokens: parsed.max_output_tokens.unwrap_or(10_000),
                })
            }
            JavaScriptPresentation::Product(product) => product.parse_execute(input),
        }
    }

    fn parse_wait(
        &self,
        input: serde_json::Value,
    ) -> Result<LocalCompositionWaitInput, LocalCompositionError> {
        match self {
            JavaScriptPresentation::CodexExact | JavaScriptPresentation::CodexMinimal { .. } => {
                let input: WaitWireInput = serde_json::from_value(input).map_err(|error| {
                    LocalCompositionError::with_source("invalid Code Mode wait input", error)
                })?;
                Ok(LocalCompositionWaitInput {
                    cell_id: input.cell_id,
                    yield_time_ms: input
                        .yield_time_ms
                        .map_or(Ok(10_000), |value| whole_u64(value, "yield_time_ms"))?
                        .min(600_000),
                    max_output_tokens: input.max_tokens.map_or(Ok(10_000), |value| {
                        whole_u64(value, "max_tokens").and_then(|value| {
                            usize::try_from(value).map_err(|error| {
                                LocalCompositionError::with_source(
                                    "Code Mode wait max_tokens exceeds the host integer boundary",
                                    error,
                                )
                            })
                        })
                    })?,
                    terminate: input.terminate.unwrap_or(false),
                })
            }
            JavaScriptPresentation::Product(product) => product.parse_wait(input),
        }
    }

    fn worker(&self) -> (WorkerIdentity, EngineConfig) {
        (QUICKJS_WORKER, EngineConfig::QUICKJS)
    }

    fn verify_worker(&self, path: &Path, identity: &[u8]) -> Result<(), LocalCompositionError> {
        verify_cell_worker_before_spawn(CellEngine::Quickjs, path, identity)
    }

    fn session_engine(&self) -> Box<dyn SessionEngine> {
        Box::new(Quickjs)
    }

    fn rebuild_descriptor(
        &self,
        job_config: plakous_tools::JobConfig,
        registry: FrozenToolRegistry,
        target: ProviderModelProjectionIdentityV2,
    ) -> Result<LocalCompositionDescriptor, LocalCompositionError> {
        match self {
            JavaScriptPresentation::CodexExact => {
                CodexNativeLocalCodeModeV1::compile(job_config, registry, target)
                    .map_err(LocalCompositionError::from_source)
                    .and_then(|value| neutral_descriptor(&value))
            }
            JavaScriptPresentation::CodexMinimal { nested_catalog } => {
                CodexMinimalLocalCodeModeV1::compile_for_catalog(
                    job_config,
                    registry,
                    target,
                    nested_catalog,
                )
                .map_err(LocalCompositionError::from_source)
                .and_then(|value| neutral_minimal_descriptor(&value))
            }
            JavaScriptPresentation::Product(product) => {
                ProductJavaScriptCompositionV1::compile_for_kind(
                    job_config,
                    registry,
                    target,
                    product.presentation_kind,
                    product.nested_names.iter().cloned(),
                )
                .map_err(LocalCompositionError::from_source)
                .and_then(|descriptor| neutral_product_descriptor(&descriptor))
            }
        }
    }

    fn validate_configuration(
        &self,
        job_config: plakous_tools::JobConfig,
        descriptor: &LocalCompositionDescriptor,
        projection: &ToolProjectionMap,
        registry: FrozenToolRegistry,
        configuration: &CompiledProviderConfiguration,
    ) -> Result<(), LocalCompositionError> {
        match self {
            JavaScriptPresentation::CodexExact => {
                let exact = CodexNativeLocalCodeModeV1::compile(
                    job_config,
                    registry.clone(),
                    descriptor.outer_projection.target().clone(),
                )
                .map_err(LocalCompositionError::from_source)?;
                if neutral_descriptor(&exact)?.identity != descriptor.identity {
                    return Err(
                        "neutral descriptor disagrees with exact provider descriptor".into(),
                    );
                }
                validate_codex_native_local_code_mode_candidate_v1(
                    &exact,
                    projection,
                    registry,
                    configuration,
                )
                .map_err(LocalCompositionError::from_source)
            }
            JavaScriptPresentation::CodexMinimal { nested_catalog } => {
                let exact = CodexMinimalLocalCodeModeV1::compile_for_catalog(
                    job_config,
                    registry.clone(),
                    descriptor.outer_projection.target().clone(),
                    nested_catalog,
                )
                .map_err(LocalCompositionError::from_source)?;
                if neutral_minimal_descriptor(&exact)?.identity != descriptor.identity {
                    return Err(
                        "neutral descriptor disagrees with minimal provider descriptor".into(),
                    );
                }
                validate_codex_minimal_local_code_mode_candidate_v1(
                    &exact,
                    projection,
                    registry,
                    configuration,
                )
                .map_err(LocalCompositionError::from_source)
            }
            JavaScriptPresentation::Product(product) => validate_product_configuration(
                job_config,
                descriptor,
                projection,
                registry,
                configuration,
                &product.nested_names,
                product.presentation_kind,
                &product.evidence,
            ),
        }
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
        match self {
            JavaScriptPresentation::Product(product) => product.limits.max_yield_time_ms,
            JavaScriptPresentation::CodexExact | JavaScriptPresentation::CodexMinimal { .. } => {
                native::MAX_YIELD_TIME_MS
            }
        }
    }

    fn max_output_tokens(&self) -> usize {
        match self {
            JavaScriptPresentation::Product(product) => product.max_output_tokens,
            JavaScriptPresentation::CodexExact | JavaScriptPresentation::CodexMinimal { .. } => {
                native::MAX_OUTPUT_TOKENS
            }
        }
    }
}

impl ProductJavaScript {
    fn parse_execute(
        &self,
        input: serde_json::Value,
    ) -> Result<LocalCompositionExecuteInput, LocalCompositionError> {
        let input: ProductRunWireInput = serde_json::from_value(input).map_err(|error| {
            LocalCompositionError::with_source("invalid product JavaScript run input", error)
        })?;
        if input.source.is_empty()
            || u64::try_from(input.source.len())
                .map_or(true, |length| length > self.limits.max_source_bytes)
        {
            return Err(
                "product JavaScript source must be non-empty and within its UTF-8 byte limit"
                    .into(),
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
            return Err("product JavaScript run input exceeds its evidence-bound limits".into());
        }
        Ok(LocalCompositionExecuteInput {
            source: input.source,
            yield_time_ms,
            max_output_tokens: usize::try_from(max_output_tokens).map_err(|error| {
                LocalCompositionError::with_source(
                    "product JavaScript output limit exceeds usize",
                    error,
                )
            })?,
        })
    }

    fn parse_wait(
        &self,
        input: serde_json::Value,
    ) -> Result<LocalCompositionWaitInput, LocalCompositionError> {
        let input: WaitWireInput = serde_json::from_value(input).map_err(|error| {
            LocalCompositionError::with_source("invalid product JavaScript wait input", error)
        })?;
        if input.cell_id.is_empty() {
            return Err("product JavaScript wait cell_id must not be empty".into());
        }
        let yield_time_ms = input
            .yield_time_ms
            .map_or(Ok(PRODUCT_JAVASCRIPT_DEFAULT_YIELD_TIME_MS), |value| {
                whole_u64(value, "yield-time_ms")
            })?
            .min(600_000);
        let default_output_tokens = usize::try_from(PRODUCT_JAVASCRIPT_DEFAULT_OUTPUT_TOKENS)
            .map_err(|error| {
                LocalCompositionError::with_source(
                    "product JavaScript default output limit exceeds usize",
                    error,
                )
            })?;
        let max_output_tokens = input
            .max_tokens
            .map_or(Ok(default_output_tokens), |value| {
                whole_u64(value, "max_tokens").and_then(|value| {
                    usize::try_from(value).map_err(|error| {
                        LocalCompositionError::with_source(
                            "product JavaScript wait max_tokens exceeds usize",
                            error,
                        )
                    })
                })
            })?;
        if max_output_tokens == 0 || max_output_tokens > self.max_output_tokens {
            return Err("product JavaScript wait input exceeds its evidence-bound limits".into());
        }
        Ok(LocalCompositionWaitInput {
            cell_id: input.cell_id,
            yield_time_ms,
            max_output_tokens,
            terminate: input.terminate.unwrap_or(false),
        })
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "presentation regressions")]
mod tests;
