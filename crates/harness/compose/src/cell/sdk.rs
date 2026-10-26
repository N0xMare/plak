//! The root-only typed Python SDK presentation, built by the shared compiler.

use plakous_model::{Digest, ToolName};
use plakous_tools::{
    FrozenToolRegistry, JobConfig, ProviderModelProjectionIdentityV2, ToolProjectionMap,
    ToolRegistryBuilder, ToolRuntimeError,
};
use std::sync::Arc;

use super::{
    compile::{CompiledCell, NestedProjection, compile_cell},
    host_api::HostApi,
    presentation::{
        CatalogStyle, CellLifecycle, CellLimits, CellMessages, CellPresentation, OuterInput,
        OuterTool, YieldMode,
    },
};

const TOOLS: &[&str] = &[
    "read_file",
    "list_entries",
    "search_repository",
    "edit_files",
    "process_session_exec",
    "process_session_start",
    "process_session_poll",
    "process_session_write",
    "process_session_terminate",
    "update_plan",
];
const MESSAGES: CellMessages = CellMessages {
    wait: "",
    custom_execute_prefix: None,
    target_refused: "Python SDK target class is not admitted",
    catalog_bound_refused: "Python SDK catalog membership is not exact",
    collision_refused: "Python SDK nested catalog collision",
};

/// Compiler for one typed, foreground Python SDK cell presentation.
pub struct PythonHarnessSdkV1;

impl PythonHarnessSdkV1 {
    /// Enrolls only the SDK extensions and its driver marker. Register the
    /// ordinary coding packs with the same process-session owner first.
    ///
    /// # Errors
    /// Returns a registration, collision or executor-definition error.
    pub fn register_into(
        registry: &mut ToolRegistryBuilder,
        process: Arc<plakous_tool_process::ProcessSessionTools>,
    ) -> Result<(), ToolRuntimeError> {
        plakous_tools::register_python_sdk_marker_into(registry)?;
        registry.register(plakous_tool_files::FilesToolPack::edit_files_registration()?)?;
        registry.register(plakous_tool_search::SearchToolPack::list_entries_registration()?)?;
        registry.register(process.exec_registration()?)
    }

    /// Compiles the SDK's fixed nested catalog and one `run_py` tool.
    ///
    /// # Errors
    /// Rejects missing capabilities, recursion, target or projection drift.
    pub fn compile(
        job_config: JobConfig,
        registry: FrozenToolRegistry,
        target: ProviderModelProjectionIdentityV2,
    ) -> Result<PythonHarnessSdkDescriptorV1, ToolRuntimeError> {
        let names = TOOLS
            .iter()
            .map(|name| ToolName::new(*name).map_err(super::compile::definition_error))
            .collect::<Result<Vec<_>, _>>()?;
        let presentation = CellPresentation {
            profile: "PythonHarnessSdkV1",
            execute: OuterTool {
                name: "run_py",
                marker: "run_py",
                input_field: Some("code"),
                input: OuterInput::Structured(plakous_tools::object_schema(
                    serde_json::json!({"code":{"type":"string","minLength":1,"maxLength":262_144}}),
                    &["code"],
                )),
            },
            wait: None,
            yield_mode: YieldMode::Never,
            lifecycle: CellLifecycle::AgentIncarnationPersistent,
            result: "BytesRunPyV1",
            limits: CellLimits::PythonSdk {
                source_bytes: 262_144,
                foreground_ms: 300_000,
                result_bytes: 1_048_576,
                cell_bytes: 16_777_216,
                consumes_values: true,
            },
            catalog: CatalogStyle::PythonSdk,
            messages: &MESSAGES,
            revisions: &["plakous.python-sdk.v1"],
            marker_executor: plakous_tools::PYTHON_SDK_DRIVER_MARKER_EXECUTOR_ID_V1,
        };
        let CompiledCell {
            identity,
            outer_projection,
            nested_projection,
            api,
            ..
        } = compile_cell(
            &presentation,
            job_config,
            registry,
            target,
            &names,
            NestedProjection::Registered,
        )?;
        Ok(PythonHarnessSdkDescriptorV1 {
            identity,
            outer_projection,
            nested_projection,
            api,
            job_config,
        })
    }
}

/// Exact compiler output for the typed SDK. Cannot be forged by a consumer.
///
/// ```compile_fail,E0451
/// use plakous_compose::PythonHarnessSdkDescriptorV1;
/// fn forge(descriptor: PythonHarnessSdkDescriptorV1) -> PythonHarnessSdkDescriptorV1 {
///     PythonHarnessSdkDescriptorV1 { identity: descriptor.identity(), ..descriptor }
/// }
/// ```
#[derive(Clone)]
pub struct PythonHarnessSdkDescriptorV1 {
    identity: Digest,
    outer_projection: ToolProjectionMap,
    nested_projection: ToolProjectionMap,
    api: HostApi,
    job_config: JobConfig,
}
impl PythonHarnessSdkDescriptorV1 {
    /// Complete v3 presentation identity.
    #[must_use]
    pub const fn identity(&self) -> Digest {
        self.identity
    }
    /// Provider-visible `run_py` projection.
    #[must_use]
    pub const fn outer_projection(&self) -> &ToolProjectionMap {
        &self.outer_projection
    }
    /// Executor-bound canonical nested capabilities.
    #[must_use]
    pub const fn nested_projection(&self) -> &ToolProjectionMap {
        &self.nested_projection
    }
    /// The SDK catalog used by all Python generators and validators.
    #[must_use]
    pub const fn host_api(&self) -> &HostApi {
        &self.api
    }
    /// Construction-bound process budgets.
    #[must_use]
    pub const fn job_config(&self) -> JobConfig {
        self.job_config
    }
}
