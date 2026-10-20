//! The neutral Python SDK composition engine (`py-harness-sdk`, spec 01 §9.8).
//!
//! One `run_py` call runs one cell to a terminal: source bound, a fresh
//! checkout of the warm worker type-checked against the generated stubs, the
//! bootstrap, then the cell, whose awaited SDK functions become canonical
//! nested children through the driver. There is no wait and no yield.
//!
//! The compiled descriptor (stubs, bootstrap, limits) comes from the harness
//! compiler; the facade passes it in as a [`HarnessSdkProgram`] and supplies
//! descriptor reconstruction through [`HarnessSdkDescriptorAuthority`].

mod args;
mod cell;
mod evidence;
mod host;
mod process;
mod render;
mod scope;
mod session;
mod values;

#[cfg(test)]
mod tests;

use std::{future::Future, path::PathBuf, pin::Pin, sync::Arc};

use plakous_agent::internal::{
    LocalCompositionContentItem, LocalCompositionDescriptor, LocalCompositionExecuteInput,
    LocalCompositionFuture, LocalCompositionIntegration, LocalCompositionLifecycle,
    LocalCompositionResultProjection, LocalCompositionRuntimeContract,
    LocalCompositionSessionBundle, LocalCompositionShutdownResult, LocalCompositionWaitInput,
    NamedStatePort,
};
use plakous_model::CompiledProviderConfiguration;
use plakous_tools::{
    FrozenToolRegistry, JobConfig, ProviderModelProjectionIdentityV2, PythonHarnessSdkLimitsV1,
    ToolProjectionMap,
};
use serde_json::Value;

/// Identity of this engine's delivery contract: its host functions, value
/// conversion, limits enforcement and observation rendering. A change to any
/// of them revises it, and with it the engine contract a session records.
pub const HARNESS_SDK_ENGINE_REVISION: &str = "plakous.monty-python-harness-sdk-engine.v2";

/// The compiled program one SDK evaluator runs: the harness compiler's stubs,
/// bootstrap, `_sdk_host` function names and limits.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HarnessSdkProgram {
    /// The `.pyi` stub text, the type checker's input.
    pub stubs: String,
    /// The bootstrap program, fed with type checking skipped.
    pub bootstrap: String,
    /// The `_sdk_host` functions the bootstrap may bind; no other is answered.
    pub host_functions: Vec<String>,
    /// The identity-bearing limits the engine enforces.
    pub limits: PythonHarnessSdkLimitsV1,
}

/// Descriptor reconstruction and configuration validation, which live with
/// the harness compiler above this engine.
pub trait HarnessSdkDescriptorAuthority: Send + Sync {
    /// Recompiles the neutral descriptor for a reopened or rebuilt agent.
    ///
    /// # Errors
    ///
    /// Returns the compiler's refusal.
    fn rebuild_descriptor(
        &self,
        job_config: JobConfig,
        registry: FrozenToolRegistry,
        target: ProviderModelProjectionIdentityV2,
    ) -> Result<LocalCompositionDescriptor, String>;

    /// Proves the attached descriptor matches the configuration.
    ///
    /// # Errors
    ///
    /// Returns why the configuration is not admitted.
    fn validate_configuration(
        &self,
        job_config: JobConfig,
        descriptor: &LocalCompositionDescriptor,
        projection: &ToolProjectionMap,
        registry: FrozenToolRegistry,
        configuration: &CompiledProviderConfiguration,
    ) -> Result<(), String>;
}

/// The `PythonHarnessSdkV1` local-composition integration of one agent
/// incarnation. Its named state and id tables outlive every generation.
pub struct HarnessSdkIntegration {
    worker: PathBuf,
    program: Arc<HarnessSdkProgram>,
    environment_identity: Vec<u8>,
    authority: Arc<dyn HarnessSdkDescriptorAuthority>,
    state: Arc<dyn NamedStatePort>,
    evidence: Arc<evidence::EvidenceTable>,
}

impl HarnessSdkIntegration {
    /// Builds the integration for one agent incarnation. `state` is that
    /// incarnation's named-state port.
    #[must_use]
    pub fn new(
        worker: PathBuf,
        program: HarnessSdkProgram,
        environment_identity: Vec<u8>,
        authority: Arc<dyn HarnessSdkDescriptorAuthority>,
        state: Arc<dyn NamedStatePort>,
    ) -> Self {
        Self {
            worker,
            program: Arc::new(program),
            environment_identity,
            authority,
            state,
            evidence: Arc::new(evidence::EvidenceTable::default()),
        }
    }
}

/// Parses `{"code": string}` and nothing else.
fn parse_code(input: &Value) -> Result<String, String> {
    match input.as_object() {
        Some(object) if object.len() == 1 => object
            .get("code")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| "run_py takes {\"code\": string}".to_owned()),
        _ => Err("run_py takes {\"code\": string}".to_owned()),
    }
}

fn whole(value: u64) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}

impl LocalCompositionIntegration for HarnessSdkIntegration {
    fn runtime_contract(&self) -> LocalCompositionRuntimeContract {
        LocalCompositionRuntimeContract {
            supports_yield: false,
        }
    }
    fn lifecycle(&self) -> LocalCompositionLifecycle {
        LocalCompositionLifecycle::AgentIncarnationPersistent
    }
    fn result_projection(&self) -> LocalCompositionResultProjection {
        LocalCompositionResultProjection::TextPythonHarnessSdkV1
    }
    fn environment_identity(&self) -> Vec<u8> {
        self.environment_identity.clone()
    }
    fn parse_execute(&self, input: Value) -> Result<LocalCompositionExecuteInput, String> {
        Ok(LocalCompositionExecuteInput {
            source: parse_code(&input)?,
            yield_time_ms: self.program.limits.foreground_budget_ms,
            max_output_tokens: whole(self.program.limits.output_bytes),
        })
    }
    fn parse_wait(&self, _input: Value) -> Result<LocalCompositionWaitInput, String> {
        Err("run_py has no wait".to_owned())
    }
    fn create_session(
        &self,
        _generation: u64,
        _max_yield_time_ms: u64,
        _max_output_tokens: usize,
    ) -> LocalCompositionFuture<'_, LocalCompositionSessionBundle> {
        session::bundle(
            &self.worker,
            Arc::clone(&self.program),
            Arc::clone(&self.evidence),
            Arc::clone(&self.state),
        )
    }
    fn shutdown(
        &self,
    ) -> Pin<Box<dyn Future<Output = LocalCompositionShutdownResult> + Send + '_>> {
        Box::pin(async { LocalCompositionShutdownResult::Joined { child_exit: None } })
    }
    fn rebuild_descriptor(
        &self,
        job_config: JobConfig,
        registry: FrozenToolRegistry,
        target: ProviderModelProjectionIdentityV2,
    ) -> Result<LocalCompositionDescriptor, String> {
        self.authority
            .rebuild_descriptor(job_config, registry, target)
    }
    fn validate_configuration(
        &self,
        job_config: JobConfig,
        descriptor: &LocalCompositionDescriptor,
        projection: &ToolProjectionMap,
        registry: FrozenToolRegistry,
        configuration: &CompiledProviderConfiguration,
    ) -> Result<(), String> {
        self.authority.validate_configuration(
            job_config,
            descriptor,
            projection,
            registry,
            configuration,
        )
    }
    /// The engine already bounded the observation to its limits.
    fn truncate_items(
        &self,
        items: Vec<LocalCompositionContentItem>,
        _max_output_tokens: usize,
    ) -> Vec<LocalCompositionContentItem> {
        items
    }
    fn max_yield_time_ms(&self) -> u64 {
        self.program.limits.foreground_budget_ms
    }
    fn max_output_tokens(&self) -> usize {
        whole(self.program.limits.output_bytes)
    }
    fn max_outstanding_nested_calls(&self) -> usize {
        self.program.limits.concurrent_nested_calls as usize
    }
}
