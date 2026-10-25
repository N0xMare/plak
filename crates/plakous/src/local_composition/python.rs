//! The Python PTC presentation on the Monty cell worker.

#![allow(
    clippy::wildcard_imports,
    reason = "a split of the local composition module that shares its vocabulary"
)]

use super::*;

use std::sync::atomic::{AtomicBool, Ordering};

use super::integration::Presentation;
use super::session::{SessionCore, SessionEngine, SessionFuture, child_exit, response};
use plakous_agent::internal::{
    ChildProcessExit, ChildProcessKind, LocalCompositionExecuteRequest, LocalCompositionResponse,
    LocalCompositionSessionFuture, LocalCompositionSessionLoss, LocalCompositionStartedCell,
    LocalCompositionTerminateResult, LocalCompositionWaitOutcome,
};
use plakous_cell::{CellPrelude, MontyBootstrap, ObservationMode, WaitOutcome};
use plakous_compose::{PythonBootstrap, PythonInput, python_bootstrap};

/// Bound on one uninterrupted stretch of Python computation: from a start, a
/// nested call or an accepted result to the next nested call, wait or
/// terminal. It is not a per-cell or interpreter-lifetime budget; operation
/// and root-turn deadlines live in the driver.
pub(super) const PYTHON_WORKER_REQUEST_BOUND: std::time::Duration =
    std::time::Duration::from_secs(90);

pub(super) fn python_presentation(
    descriptor: &PythonPtcDescriptorV1,
    evidence: ProductExtensionEvidence,
    presentation_kind: plakous_tools::CodingPresentation,
) -> Result<PythonPresentation, AgentError> {
    let nested_names = descriptor
        .nested_tools()
        .iter()
        .map(|tool| tool.canonical_name().clone())
        .collect::<Vec<_>>();
    let bootstrap = python_bootstrap(descriptor.host_api())
        .map_err(invalid)?
        .map(worker_bootstrap);
    Ok(PythonPresentation {
        nested_names: nested_names.into(),
        bootstrap,
        presentation: descriptor.presentation(),
        presentation_kind,
        evidence,
    })
}

pub(super) fn worker_bootstrap(bootstrap: PythonBootstrap) -> MontyBootstrap {
    MontyBootstrap {
        source: bootstrap.source,
        bindings: bootstrap
            .bindings
            .into_iter()
            .map(|binding| plakous_cell::MontyBinding {
                function: binding.function,
                tool: binding.tool,
                input: match binding.input {
                    PythonInput::Dictionary => plakous_cell::BindingInput::Dictionary,
                    PythonInput::String => plakous_cell::BindingInput::String,
                },
            })
            .collect(),
    }
}

/// Python PTC exec_py cell, prefix before the active cell id in a concurrent-start refusal.
const PLAKOUS_PY_PTC_ERROR_CONCURRENT_CELL_PREFIX: &str = "cell ";

/// Python PTC exec_py cell, suffix after the active cell id in a concurrent-start refusal.
const PLAKOUS_PY_PTC_ERROR_CONCURRENT_CELL_SUFFIX: &str =
    " is still running; wait for it to finish before starting another cell";

/// Python PTC: the nested catalog, the session bootstrap that binds it, and
/// the evidence its configuration is validated against.
pub(super) struct PythonPresentation {
    pub(super) nested_names: Arc<[ToolName]>,
    pub(super) bootstrap: Option<MontyBootstrap>,
    pub(super) presentation: plakous_compose::PythonPtcPresentationV1,
    pub(super) presentation_kind: plakous_tools::CodingPresentation,
    pub(super) evidence: ProductExtensionEvidence,
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

impl Presentation for PythonPresentation {
    fn runtime_contract(&self) -> LocalCompositionRuntimeContract {
        LocalCompositionRuntimeContract::PYTHON
    }

    fn lifecycle(&self) -> LocalCompositionLifecycle {
        LocalCompositionLifecycle::AgentIncarnationPersistent
    }

    fn result_projection(&self) -> LocalCompositionResultProjection {
        LocalCompositionResultProjection::BytesPythonPtcV1
    }

    fn evidence(&self) -> Option<&ProductExtensionEvidence> {
        Some(&self.evidence)
    }

    fn parse_wait(&self, input: serde_json::Value) -> Result<LocalCompositionWaitInput, String> {
        parse_python_wait(input)
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

    fn worker(&self) -> (plakous_cell::WorkerIdentity, plakous_cell::EngineConfig) {
        (
            plakous_cell::MONTY_WORKER,
            plakous_cell::EngineConfig::MONTY,
        )
    }

    fn verify_worker(&self, path: &Path, identity: &[u8]) -> Result<(), String> {
        verify_cell_worker_before_spawn(CellEngine::Monty, path, identity)
    }

    fn session_engine(&self) -> Box<dyn SessionEngine> {
        Box::new(Monty::new(
            self.bootstrap.clone(),
            PYTHON_WORKER_REQUEST_BOUND,
        ))
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

/// Monty: host-window yields; one interpreter per agent incarnation. Its
/// first cell carries the tool bootstrap.
pub(super) struct Monty {
    bootstrap: Option<MontyBootstrap>,
    request_bound: std::time::Duration,
    bootstrapped: AtomicBool,
    /// The receipt of a worker a terminate already stopped, reported by the
    /// next shutdown.
    stopped: tokio::sync::Mutex<Option<ChildProcessExit>>,
}

impl Monty {
    pub(super) fn new(
        bootstrap: Option<MontyBootstrap>,
        request_bound: std::time::Duration,
    ) -> Self {
        Self {
            bootstrap,
            request_bound,
            bootstrapped: AtomicBool::new(false),
            stopped: tokio::sync::Mutex::new(None),
        }
    }

    /// A start while another cell runs is answered, not lost: the refused
    /// start takes its own cell id and names the running one.
    async fn start_cell(
        &self,
        core: &SessionCore,
        request: LocalCompositionExecuteRequest,
    ) -> Result<LocalCompositionStartedCell, LocalCompositionSessionLoss> {
        if let Some(active) = core.session.active_cell().await {
            let refused = core.session.skip_cell();
            let response = LocalCompositionResponse::Result {
                cell_id: refused.to_string(),
                items: Vec::new(),
                error: Some(format!(
                    "{PLAKOUS_PY_PTC_ERROR_CONCURRENT_CELL_PREFIX}{active}{PLAKOUS_PY_PTC_ERROR_CONCURRENT_CELL_SUFFIX}"
                )),
            };
            return Ok(LocalCompositionStartedCell {
                cell_id: refused.to_string(),
                initial_response: Box::pin(async move { Ok(response) }),
            });
        }
        let bootstrap = if self.bootstrapped.load(Ordering::Acquire) {
            None
        } else {
            self.bootstrap.clone()
        };
        let started = core
            .start(request, CellPrelude::Monty { bootstrap })
            .await?;
        self.bootstrapped.store(true, Ordering::Release);
        Ok(started)
    }

    /// Ends a Python cell. A buffered terminal is returned as it is and keeps
    /// the interpreter; a running cell costs the interpreter, whose worker is
    /// stopped; any other id leaves everything as it was.
    async fn terminate_cell(
        &self,
        core: &SessionCore,
        cell_id: String,
    ) -> LocalCompositionTerminateResult {
        let missing = |cell_id, error| {
            LocalCompositionTerminateResult::Joined(LocalCompositionWaitOutcome::Missing(
                LocalCompositionResponse::Result {
                    cell_id,
                    items: Vec::new(),
                    error,
                },
            ))
        };
        let cell = cell_id.parse::<u64>().ok();
        if let Some(cell) = cell
            && core.session.terminal_ready(cell).await
        {
            return match core.session.wait(&cell_id, 0).await {
                WaitOutcome::Live(live) => LocalCompositionTerminateResult::Joined(
                    LocalCompositionWaitOutcome::Live(response(live)),
                ),
                WaitOutcome::Lost(loss) => LocalCompositionTerminateResult::Lost(core.loss(loss)),
                WaitOutcome::Missing { error } => missing(cell_id, error),
            };
        }
        if cell.is_none() || core.session.active_cell().await != cell {
            return missing(cell_id, None);
        }
        core.session.lose_generation();
        match core.session.stop().await {
            Ok(stop) => {
                *self.stopped.lock().await = Some(child_exit(
                    ChildProcessKind::PythonWorker,
                    stop.usage,
                    stop.forced,
                ));
                LocalCompositionTerminateResult::Joined(LocalCompositionWaitOutcome::Live(
                    LocalCompositionResponse::Terminated {
                        cell_id,
                        items: Vec::new(),
                    },
                ))
            }
            Err(loss) => LocalCompositionTerminateResult::Lost(core.loss(loss)),
        }
    }

    /// Stops the worker. A worker killed after the voluntary bound is a
    /// forced but joined stop; only one left unreaped, or already lost, is a
    /// loss.
    async fn shutdown_worker(&self, core: &SessionCore) -> LocalCompositionShutdownResult {
        if let Some(exit) = self.stopped.lock().await.take() {
            return LocalCompositionShutdownResult::Joined {
                child_exit: Some(exit),
            };
        }
        match core.session.stop().await {
            Ok(stop) => LocalCompositionShutdownResult::Joined {
                child_exit: Some(child_exit(
                    ChildProcessKind::PythonWorker,
                    stop.usage,
                    stop.forced,
                )),
            },
            Err(loss) => LocalCompositionShutdownResult::Lost(core.loss(loss)),
        }
    }
}

impl SessionEngine for Monty {
    fn mode(&self) -> ObservationMode {
        ObservationMode::HostWindow {
            request_bound: self.request_bound,
        }
    }

    fn start<'a>(
        &'a self,
        core: &'a SessionCore,
        request: LocalCompositionExecuteRequest,
    ) -> LocalCompositionSessionFuture<'a, LocalCompositionStartedCell> {
        Box::pin(self.start_cell(core, request))
    }

    fn wait_window(&self, requested: u64) -> u64 {
        requested.min(600_000)
    }

    fn terminate<'a>(
        &'a self,
        core: &'a SessionCore,
        cell_id: String,
    ) -> SessionFuture<'a, LocalCompositionTerminateResult> {
        Box::pin(self.terminate_cell(core, cell_id))
    }

    fn shutdown<'a>(
        &'a self,
        core: &'a SessionCore,
    ) -> SessionFuture<'a, LocalCompositionShutdownResult> {
        Box::pin(self.shutdown_worker(core))
    }
}

#[cfg(test)]
mod prompt_tests {
    use super::*;

    #[test]
    fn host_prompts_digest_is_stable() {
        // The worker's own messages are pinned in `plakous-py-worker`; with
        // the invalid-cell-id text the session answers, these were 353 bytes.
        let combined = [
            PLAKOUS_PY_PTC_ERROR_CONCURRENT_CELL_PREFIX,
            PLAKOUS_PY_PTC_ERROR_CONCURRENT_CELL_SUFFIX,
            "invalid cell id",
        ]
        .concat();
        assert_eq!(combined.len(), 89);
        assert!(combined.starts_with("cell "));
        assert!(!combined.contains("wait_py"));
    }
}
