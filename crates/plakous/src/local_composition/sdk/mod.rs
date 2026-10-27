//! The typed SDK presentation on the shared Python cell session.

use super::{
    ProductExtensionEvidence,
    integration::Presentation,
    python::Monty,
    session::{SessionCore, SessionEngine, SessionFuture},
};
use plakous_agent::internal::{
    LocalCompositionContentItem, LocalCompositionDescriptor, LocalCompositionEventReceiver,
    LocalCompositionExecuteInput, LocalCompositionExecuteRequest, LocalCompositionLifecycle,
    LocalCompositionResultProjection, LocalCompositionRuntimeContract,
    LocalCompositionSessionFuture, LocalCompositionSessionLoss, LocalCompositionShutdownResult,
    LocalCompositionStartedCell, LocalCompositionTerminateResult, LocalCompositionToolDefinition,
    LocalCompositionWaitInput,
};
use plakous_cell::{CellPrelude, CellSessionEvents, ObservationMode};
use plakous_compose::{PythonHarnessSdkDescriptorV1, PythonHarnessSdkV1};
use plakous_model::{CompiledProviderConfiguration, ToolExecutionDisposition};
use plakous_tools::{
    FrozenToolRegistry, JobConfig, ProviderModelProjectionIdentityV2, ToolProjectionMap,
};
use std::sync::{Arc, Mutex};

mod host;
mod refs;
mod render;
mod values;

pub(super) struct SdkPresentation {
    pub(super) budget: std::time::Duration,
    pub(super) evidence: ProductExtensionEvidence,
    pub(super) api: plakous_compose::HostApi,
}

pub(super) fn descriptor(
    exact: &PythonHarnessSdkDescriptorV1,
) -> Result<LocalCompositionDescriptor, String> {
    Ok(LocalCompositionDescriptor {
        outer_projection: exact.outer_projection().clone(),
        nested_projection: exact.nested_projection().clone(),
        nested_tools: exact
            .nested_projection()
            .entries()
            .iter()
            .map(|entry| LocalCompositionToolDefinition {
                name: entry.exposed_name().as_str().into(),
                description: entry.description().into(),
                input_schema: entry
                    .input_schema()
                    .map_or(serde_json::Value::Null, |schema| {
                        schema.document().value().as_ref().clone()
                    }),
            })
            .collect::<Vec<_>>()
            .into(),
        identity: exact.identity(),
        execute_name: "run_py".into(),
        wait_name: "".into(),
        marker_executor: plakous_tools::ToolExecutorId::new(
            plakous_tools::PYTHON_SDK_DRIVER_MARKER_EXECUTOR_ID_V1,
        )
        .map_err(|e| e.to_string())?,
    })
}

impl Presentation for SdkPresentation {
    fn foreground_budget(&self) -> Option<std::time::Duration> {
        Some(self.budget)
    }
    fn consumes_values(&self) -> bool {
        true
    }
    fn python_sdk_stubs(&self) -> Option<String> {
        self.api.python_sdk_stubs()
    }
    fn runtime_contract(&self) -> LocalCompositionRuntimeContract {
        LocalCompositionRuntimeContract::FOREGROUND
    }
    fn lifecycle(&self) -> LocalCompositionLifecycle {
        LocalCompositionLifecycle::AgentIncarnationPersistent
    }
    fn result_projection(&self) -> LocalCompositionResultProjection {
        LocalCompositionResultProjection::BytesRunPyV1
    }
    fn evidence(&self) -> Option<&ProductExtensionEvidence> {
        Some(&self.evidence)
    }
    fn parse_execute(
        &self,
        input: serde_json::Value,
    ) -> Result<LocalCompositionExecuteInput, String> {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Input {
            code: String,
        }
        let input: Input =
            serde_json::from_value(input).map_err(|_| "run_py requires exactly one code string")?;
        if input.code.is_empty()
            || input.code.len() > PythonHarnessSdkDescriptorV1::SOURCE_BYTES as usize
        {
            return Err("SDK code must be non-empty and at most 256 KiB".into());
        }
        Ok(LocalCompositionExecuteInput {
            source: input.code,
            yield_time_ms: PythonHarnessSdkDescriptorV1::FOREGROUND_MS,
            max_output_tokens: PythonHarnessSdkDescriptorV1::OUTPUT_BYTES,
        })
    }
    fn parse_wait(&self, _input: serde_json::Value) -> Result<LocalCompositionWaitInput, String> {
        Err("the SDK has no wait tool".into())
    }
    fn rebuild_descriptor(
        &self,
        job: JobConfig,
        registry: FrozenToolRegistry,
        target: ProviderModelProjectionIdentityV2,
    ) -> Result<LocalCompositionDescriptor, String> {
        descriptor(&PythonHarnessSdkV1::compile(job, registry, target).map_err(|e| e.to_string())?)
    }
    fn validate_configuration(
        &self,
        job: JobConfig,
        exact: &LocalCompositionDescriptor,
        projection: &ToolProjectionMap,
        registry: FrozenToolRegistry,
        configuration: &CompiledProviderConfiguration,
    ) -> Result<(), String> {
        if !projection.is_for_registry(&registry)
            || !exact.nested_projection.is_for_registry(&registry)
            || !exact.outer_projection.is_for_registry(&registry)
        {
            return Err("SDK projection belongs to a foreign registry".into());
        }
        let effective = configuration.effective();
        let model = effective.descriptor();
        let target = ProviderModelProjectionIdentityV2::new(
            model.integration_class(),
            model.protocol().as_str(),
            model.route().as_str(),
            model.profile().as_str(),
            model
                .model_family()
                .ok_or("SDK target lacks a model family")?
                .as_str(),
            model.requested_model().as_str(),
            effective.effective_model().as_str(),
            configuration.effective_identity(),
            plakous_tools::PROCESS_TOOL_PROJECTION_REVISION,
        )
        .map_err(|e| e.to_string())?;
        if projection.target() != &target
            || self.evidence.configuration_identities()
                != (
                    configuration.selected_identity(),
                    configuration.effective_identity(),
                )
        {
            return Err("SDK evidence names a foreign configuration".into());
        }
        let tools = &effective.behavior().tools;
        if tools.len() != 1
            || tools.iter().any(|tool| {
                tool.definition().name().as_str() != "run_py"
                    || tool.execution() != ToolExecutionDisposition::Local
            })
        {
            return Err("SDK configuration requires only run_py".into());
        }
        let compiled =
            PythonHarnessSdkV1::compile(job, registry, target).map_err(|e| e.to_string())?;
        let (_, _, nested, outer, _, _) = self.evidence.extension_facts();
        if compiled.identity() != exact.identity
            || compiled.outer_projection().digest() != projection.digest()
            || nested != compiled.nested_projection().digest()
            || outer != compiled.outer_projection().digest()
        {
            return Err("SDK descriptor, receipt or outer projection drifted".into());
        }
        Ok(())
    }
    fn truncate_items(
        &self,
        items: Vec<LocalCompositionContentItem>,
        bound: usize,
    ) -> Vec<LocalCompositionContentItem> {
        super::python::truncate_python_items(items, bound)
    }
    fn max_yield_time_ms(&self) -> u64 {
        PythonHarnessSdkDescriptorV1::FOREGROUND_MS
    }
    fn max_output_tokens(&self) -> usize {
        PythonHarnessSdkDescriptorV1::OUTPUT_BYTES
    }
    fn worker(&self) -> (plakous_cell::WorkerIdentity, plakous_cell::EngineConfig) {
        (
            plakous_cell::MONTY_WORKER,
            plakous_cell::EngineConfig::MONTY,
        )
    }
    fn verify_worker(&self, path: &std::path::Path, identity: &[u8]) -> Result<(), String> {
        super::verify_cell_worker_before_spawn(super::CellEngine::Monty, path, identity)
    }
    fn session_engine(&self) -> Box<dyn SessionEngine> {
        Box::new(SdkEngine::new(self.api.clone(), self.budget))
    }
}

struct SdkEngine {
    api: Arc<plakous_compose::HostApi>,
    tasks: Arc<host::Tasks>,
    monty: Monty,
    refs: Arc<Mutex<refs::Refs>>,
    budget: std::time::Duration,
}
impl SdkEngine {
    fn new(api: plakous_compose::HostApi, budget: std::time::Duration) -> Self {
        Self {
            api: Arc::new(api),
            tasks: Arc::default(),
            monty: Monty::new(None, super::python::PYTHON_WORKER_REQUEST_BOUND),
            refs: Arc::default(),
            budget,
        }
    }
}
impl SessionEngine for SdkEngine {
    fn events(
        &self,
        events: CellSessionEvents,
        _exec_call: Arc<Mutex<String>>,
    ) -> Box<dyn LocalCompositionEventReceiver> {
        Box::new(host::Events::new(
            events,
            Arc::clone(&self.refs),
            Arc::clone(&self.api),
            Arc::clone(&self.tasks),
        ))
    }
    fn mode(&self) -> ObservationMode {
        ObservationMode::Foreground {
            request_bound: self.budget + std::time::Duration::from_secs(60),
            budget: self.budget,
        }
    }
    fn start<'a>(
        &'a self,
        core: &'a SessionCore,
        request: LocalCompositionExecuteRequest,
    ) -> LocalCompositionSessionFuture<'a, LocalCompositionStartedCell> {
        Box::pin(async move {
            let mut started = core
                .start(
                    request,
                    CellPrelude::MontySdk {
                        bootstrap: super::python::worker_bootstrap(
                            plakous_compose::python_bootstrap(&self.api)
                                .map_err(LocalCompositionSessionLoss::ProtocolViolation)?
                                .ok_or_else(|| {
                                    LocalCompositionSessionLoss::ProtocolViolation(
                                        "SDK bootstrap is missing".into(),
                                    )
                                })?,
                        ),
                    },
                )
                .await?;
            let refs = Arc::clone(&self.refs);
            started.initial_response = Box::pin(async move {
                let mut response = started.initial_response.await?;
                render::observation(&mut response, &refs);
                Ok(response)
            });
            Ok(started)
        })
    }
    fn terminate<'a>(
        &'a self,
        core: &'a SessionCore,
        cell: String,
    ) -> SessionFuture<'a, LocalCompositionTerminateResult> {
        self.monty.terminate(core, cell)
    }
    fn shutdown<'a>(
        &'a self,
        core: &'a SessionCore,
    ) -> SessionFuture<'a, LocalCompositionShutdownResult> {
        Box::pin(async move {
            let shutdown = self.monty.shutdown(core).await;
            match self.tasks.stop().await {
                Ok(()) => shutdown,
                Err(error) => LocalCompositionShutdownResult::Lost(
                    LocalCompositionSessionLoss::HostLost(error),
                ),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn sdk_foreground_stop_is_typed_and_reaps_the_worker()
    -> Result<(), Box<dyn std::error::Error>> {
        let path = std::env::var_os("PLAK_PY_HOST").ok_or("Python worker prerequisite missing")?;
        let client = Arc::new(
            plakous_cell::CellClient::spawn_python_sdk(std::path::Path::new(&path), String::new())
                .await?,
        );
        let (session, _events) = plakous_cell::CellSession::new(
            client,
            ObservationMode::Foreground {
                request_bound: std::time::Duration::from_secs(30),
                budget: std::time::Duration::from_millis(20),
            },
        );
        let started = session
            .execute(
                "while True:\n    pass",
                CellPrelude::Monty { bootstrap: None },
                u64::MAX,
                async { Ok::<(), String>(()) },
            )
            .await
            .map_err(|e| format!("{e:?}"))?;
        let response =
            tokio::time::timeout(std::time::Duration::from_secs(10), started.initial).await??;
        assert!(
            matches!(response,plakous_cell::CellResponse::Result{error:Some(ref error),..} if error=="cell stopped: ForegroundBudget"),
            "{response:?}"
        );
        assert!(session.generation_lost());
        session.stop().await?;
        Ok(())
    }
}
