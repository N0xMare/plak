//! Exact Codex OAuth `CodeModeOnly` presentations and nested coding catalogs.
//!
//! The frozen seven-tool reference remains exact. The composition adds the
//! separately identified four-tool minimum-optimal default. Product-owned
//! nested-eligible extensions may append to the four-tool catalog through an
//! explicit compile and rotate that catalog's identity; `NestedEligible` alone
//! never implies exposure. The seven-tool reference does not accept extras.

use std::collections::BTreeSet;
use std::sync::Arc;

use plakous_model::{
    CompiledProviderConfiguration, Digest, DigestDomain, IntegrationClass,
    ProjectedCustomToolFormatV2, ProjectedToolPresentationV2, SemanticJsonV1, ToolDefinition,
    ToolEffect, ToolExecutionDisposition, ToolInputContract, ToolName, Utf8TextContractV1,
};
use serde::Serialize;
use serde_json::{Value, json};

use plakous_tool_files::codex_patch_envelope_entry;
use plakous_tool_process::ProcessToolProjectionKind;
use plakous_tools::resolve_entry;
use plakous_tools::{
    APPLY_PATCH_CAPABILITY_NAME, INSPECT_IMAGE_CAPABILITY_NAME, READ_FILE_CAPABILITY_NAME,
    SEARCH_REPOSITORY_CAPABILITY_NAME, UPDATE_PLAN_CAPABILITY_NAME, is_composition_marker_executor,
    object_schema,
};
use plakous_tools::{
    CODE_MODE_EXEC_MAX_BYTES_V1, CODEX_NATIVE_CODE_MODE_COMMIT_V1,
    CODEX_NATIVE_CODE_MODE_DRIVER_MARKER_EXECUTOR_ID_V1, CODEX_NATIVE_CODE_MODE_ENGINE_REVISION_V1,
    CODEX_NATIVE_CODE_MODE_EXEC_NAME_V1, CODEX_NATIVE_CODE_MODE_RELEASE_V1,
    CODEX_NATIVE_CODE_MODE_WAIT_NAME_V1, codex_code_mode_wait_schema,
    register_codex_code_mode_markers_into,
};
use plakous_tools::{
    CanonicalProcessOperationV2, FrozenToolRegistry, NestedToolEligibility, PortableToolSchemaV2,
    ProviderModelProjectionIdentityV2, ToolExecutorId, ToolProjectionExposureV2,
    ToolProjectionLoweringV2, ToolProjectionMap, ToolProjectionRaisingV2, ToolRegistryBuilder,
    ToolRuntimeError, ToolRuntimeErrorKind,
};

/// The one protocol the Codex Code Mode presentation is compiled for.
pub const CODEX_PROJECTION_PROTOCOL: &str = "openai.codex.responses";
/// The one route the Codex Code Mode presentation is compiled for.
pub const CODEX_PROJECTION_ROUTE: &str = "openai.codex_subscription";
/// The one integration profile the Codex Code Mode presentation is compiled for.
pub const CODEX_PROJECTION_PROFILE: &str = "openai.codex.compat.2026-07-31.r1";
/// The `openai` API protocol a Code Mode presentation is also compiled for.
pub const OPENAI_PLATFORM_PROJECTION_PROTOCOL: &str = "openai.responses";
/// The `openai` API route a Code Mode presentation is also compiled for. Which
/// profiles and models it carries is the route's own enrollment.
pub const OPENAI_PLATFORM_PROJECTION_ROUTE: &str = "openai.platform";

const CODEX_NATIVE_CODE_MODE_FAMILY_V1: &str = "responses-family";

/// Exact default initial `exec`/`wait` yield interval.
pub const CODEX_NATIVE_CODE_MODE_DEFAULT_YIELD_MS_V1: u64 = 10_000;
/// Exact default direct-result token budget for `exec` and `wait`.
pub const CODEX_NATIVE_CODE_MODE_DEFAULT_OUTPUT_TOKENS_V1: u64 = 10_000;
/// The repository harness's own enrolled Code Mode target.
///
/// Every field carries a `scripted` marker, so this can never collide with a
/// real Codex target and never lets a Codex model through a path it would not
/// otherwise take: it is an additional enrolment, not a relaxation. The offline
/// harness needs a target the enrolment check admits in order to drive a real
/// composition, and the alternative was giving a test model an identity that
/// claims to be an enrolled Codex model, which is what this check exists to
/// prevent.
#[cfg(feature = "scripted-code-mode-enrolment")]
pub const SCRIPTED_CODE_MODE_PROTOCOL_V1: &str = "offline.scripted.code-mode.responses";
/// Route of [`SCRIPTED_CODE_MODE_PROTOCOL_V1`]'s target.
#[cfg(feature = "scripted-code-mode-enrolment")]
pub const SCRIPTED_CODE_MODE_ROUTE_V1: &str = "offline.scripted.code-mode";
/// Integration profile of [`SCRIPTED_CODE_MODE_PROTOCOL_V1`]'s target.
#[cfg(feature = "scripted-code-mode-enrolment")]
pub const SCRIPTED_CODE_MODE_PROFILE_V1: &str = "offline-scripted-code-mode-adapter-v1";
/// Model family of [`SCRIPTED_CODE_MODE_PROTOCOL_V1`]'s target.
#[cfg(feature = "scripted-code-mode-enrolment")]
pub const SCRIPTED_CODE_MODE_FAMILY_V1: &str = "offline.scripted.code-mode.responses";
/// Model of [`SCRIPTED_CODE_MODE_PROTOCOL_V1`]'s target.
#[cfg(feature = "scripted-code-mode-enrolment")]
pub const SCRIPTED_CODE_MODE_MODEL_V1: &str = "offline-scripted-code-mode-v1";

/// Whether `target` is the harness's own enrolled Code Mode target.
#[cfg(feature = "scripted-code-mode-enrolment")]
fn is_scripted_code_mode_target(target: &ProviderModelProjectionIdentityV2) -> bool {
    target.protocol() == SCRIPTED_CODE_MODE_PROTOCOL_V1
        && target.route() == SCRIPTED_CODE_MODE_ROUTE_V1
        && target.integration_profile() == SCRIPTED_CODE_MODE_PROFILE_V1
        && target.family() == SCRIPTED_CODE_MODE_FAMILY_V1
        && target.requested_model() == SCRIPTED_CODE_MODE_MODEL_V1
}

/// Exact release 0.153.3 Lark grammar, including its leading and trailing newline.
pub const CODEX_NATIVE_CODE_MODE_EXEC_GRAMMAR_V1: &str = r"
start: pragma_source | plain_source
pragma_source: PRAGMA_LINE NEWLINE SOURCE
plain_source: SOURCE

PRAGMA_LINE: /[ \t]*\/\/ @exec:[^\r\n]*/
NEWLINE: /\r?\n/
SOURCE: /[\s\S]+/
";

const CODE_MODE_DESCRIPTOR_DIGEST_VERSION: u32 = 2;
const EXACT_REFERENCE_EXEC_DESCRIPTION_TEMPLATE: &str = r#"Run JavaScript code to orchestrate/compose tool calls
- Evaluates the provided JavaScript code in a fresh V8 isolate as an async module.
- All nested tools are available on the global `tools` object, for example `await tools.exec_command(...)`. Tool names are exposed as normalized JavaScript identifiers.
- Nested tool methods take either a string or an object as their input argument.
- Nested tools return either an object or a string, based on the description.
- Runs raw JavaScript -- no Node, no file system, no network access, no console.
- Accepts raw JavaScript source text, not JSON, quoted strings, or markdown code fences.
- You may optionally start the tool input with a first-line pragma like `// @exec: {"yield_time_ms": 10000, "max_output_tokens": 1000}`.
- `yield_time_ms` asks `exec` to yield early if the script is still running. Defaults to 10000 ms.
- `max_output_tokens` sets the token budget for direct `exec` results. Defaults to 10000 tokens.
- When the JS code is fully evaluated, the isolate's lifetime ends and unawaited promises are silently discarded.

- Global helpers:
- `exit()`: Immediately ends the current script successfully (like an early return from the top level).
- `text(value: string | number | boolean | undefined | null)`: Appends a text item. Non-string values are stringified with `JSON.stringify(...)` when possible.
- `store(key: string, value: any)`: stores a serializable value under a string key for later `exec` calls in the same session.
- `load(key: string)`: returns the stored value for a string key, or `undefined` if it is missing.
- `notify(value: string | number | boolean | undefined | null)`: immediately injects an extra `custom_tool_call_output` for the current `exec` call. Values are stringified like `text(...)`.
- `setTimeout(callback: () => void, delayMs?: number)`: schedules a callback to run later and returns a timeout id. Pending timeouts do not keep `exec` alive by themselves; await an explicit promise if you need to wait for one.
- `clearTimeout(timeoutId?: number)`: cancels a timeout created by `setTimeout`.
- `ALL_TOOLS`: metadata for the enabled nested tools as `{ name, description }` entries.
- `yield_control()`: yields the accumulated output to the model immediately while the script keeps running."#;
const MINIMAL_EXEC_DESCRIPTION_TEMPLATE: &str = r#"Run raw JavaScript as an async module in a fresh isolate (no Node, filesystem, network, or console). Supply source, not JSON, quoted strings, or markdown fences.
- All nested tools use the global `tools` object, e.g. `await tools.exec_command(...)`; names are normalized JavaScript identifiers. Inputs and results are strings or objects as documented.
- Optional first-line pragma: `// @exec: {"yield_time_ms": 10000, "max_output_tokens": 1000}`. `yield_time_ms` yields early while running (default 10000 ms); `max_output_tokens` bounds direct results (default 10000 tokens).
- On completion the isolate ends; unawaited promises are discarded.

Global helpers:
- `exit()`: immediately end successfully.
- `text(value: string | number | boolean | undefined | null)`: appends output. Non-string values are stringified with `JSON.stringify(...)` when possible.
- `store(key: string, value: any)` / `load(key: string)`: save serializable session values across cells; missing keys return `undefined`.
- `notify(value: string | number | boolean | undefined | null)`: immediately emits `custom_tool_call_output`, using `text` conversion.
- `setTimeout(callback: () => void, delayMs?: number)`: schedule a callback and return its timer id. Pending timers do not keep the isolate alive; await an explicit promise.
- `clearTimeout(timeoutId?: number)`: cancel a scheduled timer.
- `ALL_TOOLS`: enabled nested tool metadata `{ name, description }`.
- `yield_control()`: immediately yield accumulated output while the script continues."#;
const WAIT_DESCRIPTION: &str = "Waits on a yielded `exec` cell and returns new output or completion.\n- Use `wait` only after `exec` returns `Script running with cell ID ...`.\n- `cell_id` identifies the running `exec` cell to resume.\n- `yield_time_ms` controls how long to wait for more output before yielding again. Defaults to 10000 ms; clamped to at most 600000 ms.\n- `max_tokens` limits how much new output this wait call returns. Defaults to 10000 tokens.\n- `terminate: true` stops the running cell; false or omitted waits for output.\n- `wait` returns only the new output since the last yield, or the final completion or termination result for that cell.\n- If the cell is still running, `wait` may yield again with the same `cell_id`.\n- If the cell has already finished, `wait` returns the completed result and closes the cell.";
const EXACT_REFERENCE_NESTED_NAMES: [&str; 7] = [
    "exec_command",
    "write_stdin",
    "apply_patch",
    "update_plan",
    "read_file",
    "search_repository",
    "inspect_image",
];
const MINIMAL_CORE_NESTED_NAMES: [&str; 4] =
    ["exec_command", "write_stdin", "apply_patch", "update_plan"];

/// Exact `CodeModeOnly` outer presentation kind.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CodexNativeLocalCodeModeV1;

impl CodexNativeLocalCodeModeV1 {
    /// Exact seven model-visible nested tool names in frozen sidecar order.
    #[must_use]
    pub const fn nested_names() -> &'static [&'static str] {
        &EXACT_REFERENCE_NESTED_NAMES
    }

    /// Registers the authority-free `exec` and `wait` marker definitions.
    ///
    /// Both definitions share one explicit executor/binding identity. Ordinary
    /// execution always fails closed; the future private driver must recognize
    /// and mediate this binding before dispatch.
    ///
    /// # Errors
    ///
    /// Returns the registry's normal definition, duplicate, or capacity error.
    pub fn register_markers_into(
        registry: &mut ToolRegistryBuilder,
    ) -> Result<(), ToolRuntimeError> {
        register_codex_code_mode_markers_into(registry)
    }

    /// Compiles the exact outer presentation and nested catalog over one frozen registry.
    ///
    /// # Errors
    ///
    /// Rejects missing/drifted marker or coding registrations, any name
    /// collision, or an invalid exact projection.
    pub fn compile(
        job_config: plakous_tools::JobConfig,
        registry: FrozenToolRegistry,
        target: ProviderModelProjectionIdentityV2,
    ) -> Result<CodexNativeLocalCodeModeDescriptorV1, ToolRuntimeError> {
        compile_code_mode_descriptor(
            job_config,
            registry,
            target,
            &EXACT_REFERENCE_NESTED_NAMES,
            true,
            &[],
            b"plakous-tools/codex-native-local-code-mode-v1",
        )
    }
}

/// Codex-native Code Mode over the minimum-optimal Codex OAuth tool catalog.
///
/// This retains the pinned `exec`/`wait` wire and evaluator contract while
/// exposing only the four tools in the ordinary Codex OAuth presentation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CodexMinimalLocalCodeModeV1;

impl CodexMinimalLocalCodeModeV1 {
    /// Four model-visible nested tools in the familiar Codex order.
    #[must_use]
    pub const fn nested_names() -> &'static [&'static str] {
        &MINIMAL_CORE_NESTED_NAMES
    }

    /// Registers the shared authority-free `exec` and `wait` markers.
    ///
    /// # Errors
    ///
    /// Returns the registry's normal definition, duplicate, or capacity error.
    pub fn register_markers_into(
        registry: &mut ToolRegistryBuilder,
    ) -> Result<(), ToolRuntimeError> {
        CodexNativeLocalCodeModeV1::register_markers_into(registry)
    }

    /// Compiles the reduced native projection over the Codex core catalog.
    ///
    /// # Errors
    ///
    /// Rejects missing or drifted core registrations, marker mismatches, name
    /// collisions, and invalid projections.
    pub fn compile(
        job_config: plakous_tools::JobConfig,
        registry: FrozenToolRegistry,
        target: ProviderModelProjectionIdentityV2,
    ) -> Result<CodexMinimalLocalCodeModeDescriptorV1, ToolRuntimeError> {
        Self::compile_with_extensions(job_config, registry, target, &[])
    }

    /// Compiles the four-tool core plus an explicit ordered product nested catalog.
    ///
    /// Empty extras preserve the four-tool identity. Nonempty extras rotate the
    /// identity label; registration or `NestedEligible` alone never selects them.
    ///
    /// # Errors
    ///
    /// Rejects missing or drifted core registrations, composition-marker
    /// extras, collisions with the core four, duplicate extras, marker
    /// mismatches, name collisions, and invalid projections.
    pub fn compile_with_extensions(
        job_config: plakous_tools::JobConfig,
        registry: FrozenToolRegistry,
        target: ProviderModelProjectionIdentityV2,
        extra_nested: &[ToolName],
    ) -> Result<CodexMinimalLocalCodeModeDescriptorV1, ToolRuntimeError> {
        let mut extra_strs = Vec::with_capacity(extra_nested.len());
        let mut seen = BTreeSet::new();
        for extra in extra_nested {
            let name = extra.as_str();
            if MINIMAL_CORE_NESTED_NAMES.contains(&name) || !seen.insert(name) {
                return Err(ToolRuntimeError::new(
                    ToolRuntimeErrorKind::Registration,
                    "product Code Mode extension collides with the core catalog or is duplicated",
                ));
            }
            extra_strs.push(name);
        }
        let mut nested_names =
            Vec::with_capacity(MINIMAL_CORE_NESTED_NAMES.len() + extra_strs.len());
        nested_names.extend(MINIMAL_CORE_NESTED_NAMES);
        nested_names.extend_from_slice(&extra_strs);
        Self::compile_for_catalog(
            job_config,
            registry,
            target,
            &nested_names
                .iter()
                .map(|name| ToolName::new(*name))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| {
                    ToolRuntimeError::new(ToolRuntimeErrorKind::Registration, error.to_string())
                })?,
        )
    }

    /// Compiles the Codex core with optional file writing and explicit extensions.
    ///
    /// # Errors
    /// Rejects missing process/plan tools, duplicates,
    /// recursive markers, and invalid or ineligible extensions.
    pub fn compile_for_catalog(
        job_config: plakous_tools::JobConfig,
        registry: FrozenToolRegistry,
        target: ProviderModelProjectionIdentityV2,
        catalog: &[ToolName],
    ) -> Result<CodexMinimalLocalCodeModeDescriptorV1, ToolRuntimeError> {
        let nested_names = catalog.iter().map(ToolName::as_str).collect::<Vec<_>>();
        let core = MINIMAL_CORE_NESTED_NAMES
            .iter()
            .copied()
            .filter(|name| nested_names.contains(name))
            .collect::<Vec<_>>();
        if core != MINIMAL_CORE_NESTED_NAMES
            && core != ["exec_command", "write_stdin", "update_plan"]
        {
            return Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::Registration,
                "Code Mode catalog must retain ordered process and plan tools",
            ));
        }
        let mut seen = BTreeSet::new();
        if nested_names.iter().any(|name| !seen.insert(*name)) {
            return Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::Registration,
                "Code Mode catalog is duplicated",
            ));
        }
        let extra_strs = nested_names
            .iter()
            .copied()
            .filter(|name| !MINIMAL_CORE_NESTED_NAMES.contains(name))
            .collect::<Vec<_>>();
        let nested_names = core.iter().chain(&extra_strs).copied().collect::<Vec<_>>();
        let identity_label = if extra_strs.is_empty() {
            b"plakous-tools/codex-minimal-local-code-mode-v1".as_slice()
        } else {
            b"plakous-tools/codex-minimal-local-code-mode-v1.subagents.v1".as_slice()
        };
        compile_code_mode_descriptor(
            job_config,
            registry,
            target,
            &nested_names,
            false,
            &extra_strs,
            identity_label,
        )
        .map(CodexMinimalLocalCodeModeDescriptorV1)
    }
}

fn compile_code_mode_descriptor(
    job_config: plakous_tools::JobConfig,
    registry: FrozenToolRegistry,
    target: ProviderModelProjectionIdentityV2,
    nested_names: &[&str],
    include_extensions: bool,
    extra_nested: &[&str],
    identity_label: &[u8],
) -> Result<CodexNativeLocalCodeModeDescriptorV1, ToolRuntimeError> {
    let nested_projection = nested_projection(
        job_config,
        registry.clone(),
        target.clone(),
        include_extensions,
        extra_nested,
    )?;
    let nested_projection = plakous_tools::internal::projection_map(
        registry.clone(),
        target.clone(),
        nested_projection
            .entries()
            .iter()
            .filter(|entry| nested_names.contains(&entry.exposed_name().as_str()))
            .cloned(),
    )?;
    validate_nested_eligibility(&nested_projection, nested_names)?;
    let nested_tools = ordered_nested_catalog(&nested_projection, nested_names)?;
    validate_nested_catalog(&nested_tools, nested_names)?;
    let exec_description = build_exec_description(&nested_tools, include_extensions);
    let wait_description = WAIT_DESCRIPTION.to_owned();
    let outer_projection = outer_projection(
        registry,
        target,
        exec_description.clone(),
        wait_description.clone(),
    )?;
    let identity = descriptor_identity(
        &outer_projection,
        &nested_projection,
        &nested_tools,
        &exec_description,
        &wait_description,
        identity_label,
    )?;
    Ok(CodexNativeLocalCodeModeDescriptorV1 {
        job_config,
        outer_projection,
        nested_projection,
        nested_tools: nested_tools.into(),
        exec_description: exec_description.into(),
        wait_description: wait_description.into(),
        identity,
    })
}

fn validate_nested_eligibility(
    projection: &ToolProjectionMap,
    nested_names: &[&str],
) -> Result<(), ToolRuntimeError> {
    for name in nested_names {
        let exposed = ToolName::new(name.to_owned())
            .map_err(|error| definition_error("nested Code Mode name is invalid", &error))?;
        let entry = projection.by_exposed(&exposed).ok_or_else(|| {
            ToolRuntimeError::new(
                ToolRuntimeErrorKind::Registration,
                "required nested Code Mode capability is not projected",
            )
        })?;
        // The outer `exec`/`wait` markers own the outer namespace only; a
        // nested `wait` of another slot is not recursion, a marker is.
        if entry
            .canonical_operations()
            .values()
            .any(|identity| is_composition_marker_executor(identity.executor_id()))
        {
            return Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::Registration,
                "composition outer markers cannot enter a nested catalog",
            ));
        }
        if entry
            .canonical_operations()
            .values()
            .any(|identity| identity.nested_eligibility() != NestedToolEligibility::NestedEligible)
        {
            return Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::Registration,
                "required nested Code Mode capability is not explicitly nested-eligible",
            ));
        }
    }
    Ok(())
}

/// Reconstruction-complete descriptor for minimal Codex-native Code Mode.
#[derive(Clone)]
pub struct CodexMinimalLocalCodeModeDescriptorV1(CodexNativeLocalCodeModeDescriptorV1);

impl CodexMinimalLocalCodeModeDescriptorV1 {
    /// Construction-bound process budgets used by the nested catalog.
    #[must_use]
    pub const fn job_config(&self) -> plakous_tools::JobConfig {
        self.0.job_config()
    }

    /// Provider-visible native `exec`/`wait` projection.
    #[must_use]
    pub const fn outer_projection(&self) -> &ToolProjectionMap {
        self.0.outer_projection()
    }

    /// Core catalog plus any explicit product extensions, routed through canonical executors.
    #[must_use]
    pub const fn nested_projection(&self) -> &ToolProjectionMap {
        self.0.nested_projection()
    }

    /// Product-owned nested names after the four-tool Codex OAuth core.
    #[must_use]
    pub fn extra_nested_names(&self) -> Vec<ToolName> {
        extra_nested_names(&self.0)
    }

    /// Minimal sidecar catalog in stable JavaScript-global order.
    #[must_use]
    pub fn nested_tools(&self) -> &[CodexCodeModeNestedToolV1] {
        self.0.nested_tools()
    }

    /// Provider-visible `exec` description containing the reduced catalog.
    #[must_use]
    pub fn exec_description(&self) -> &str {
        self.0.exec_description()
    }

    /// Provider-visible `wait` description.
    #[must_use]
    pub fn wait_description(&self) -> &str {
        self.0.wait_description()
    }

    /// Identity binding the reduced catalog and exact native outer contract.
    #[must_use]
    pub const fn identity(&self) -> Digest {
        self.0.identity()
    }
}

/// Reconstruction-complete tools-owned Code Mode presentation descriptor.
#[derive(Clone)]
pub struct CodexNativeLocalCodeModeDescriptorV1 {
    job_config: plakous_tools::JobConfig,
    outer_projection: ToolProjectionMap,
    nested_projection: ToolProjectionMap,
    nested_tools: Arc<[CodexCodeModeNestedToolV1]>,
    exec_description: Arc<str>,
    wait_description: Arc<str>,
    identity: Digest,
}

impl CodexNativeLocalCodeModeDescriptorV1 {
    /// Construction-bound process budgets used by the nested catalog.
    #[must_use]
    pub const fn job_config(&self) -> plakous_tools::JobConfig {
        self.job_config
    }

    /// The exact provider-visible `exec`/`wait` projection.
    #[must_use]
    pub const fn outer_projection(&self) -> &ToolProjectionMap {
        &self.outer_projection
    }

    /// The seven-tool local nested projection routed through canonical executors.
    #[must_use]
    pub const fn nested_projection(&self) -> &ToolProjectionMap {
        &self.nested_projection
    }

    /// Exact sidecar catalog in stable JavaScript-global order.
    #[must_use]
    pub fn nested_tools(&self) -> &[CodexCodeModeNestedToolV1] {
        &self.nested_tools
    }

    /// Exact release-generated provider-visible `exec` description.
    #[must_use]
    pub fn exec_description(&self) -> &str {
        &self.exec_description
    }

    /// Exact release-generated provider-visible `wait` description.
    #[must_use]
    pub fn wait_description(&self) -> &str {
        &self.wait_description
    }

    /// Complete identity binding outer bytes, nested order/schemas, grammar, and defaults.
    #[must_use]
    pub const fn identity(&self) -> Digest {
        self.identity
    }

    /// Whether an enrolled executor identity is the driver-mediated marker.
    #[must_use]
    pub fn is_driver_marker_executor(executor: &ToolExecutorId) -> bool {
        executor.as_str() == CODEX_NATIVE_CODE_MODE_DRIVER_MARKER_EXECUTOR_ID_V1
    }
}

/// Validates one exact Codex-native local Code Mode construction candidate.
///
/// This is deliberately separate from the direct process-presentation
/// validator: Code Mode projects the provider-visible `exec`/`wait` markers
/// while its seven canonical coding capabilities remain nested behind the
/// resident host.
///
/// # Errors
///
/// Rejects every provider tuple, effective-configuration, registry, marker,
/// descriptor, or selected outer-projection mismatch without fallback.
pub fn validate_codex_native_local_code_mode_candidate_v1(
    descriptor: &CodexNativeLocalCodeModeDescriptorV1,
    projection: &ToolProjectionMap,
    registry: FrozenToolRegistry,
    configuration: &CompiledProviderConfiguration,
) -> Result<(), ToolRuntimeError> {
    validate_codex_code_mode_candidate_v1(descriptor, projection, registry, configuration, false)
}

/// Validates the minimum-optimal Codex-native local Code Mode candidate.
///
/// # Errors
///
/// Rejects provider, configuration, registry, marker, descriptor, or selected
/// projection drift before provider or evaluator dispatch.
pub fn validate_codex_minimal_local_code_mode_candidate_v1(
    descriptor: &CodexMinimalLocalCodeModeDescriptorV1,
    projection: &ToolProjectionMap,
    registry: FrozenToolRegistry,
    configuration: &CompiledProviderConfiguration,
) -> Result<(), ToolRuntimeError> {
    validate_codex_code_mode_candidate_v1(&descriptor.0, projection, registry, configuration, true)
}

#[allow(
    clippy::too_many_lines,
    reason = "exact candidate qualification is one closed comparison"
)]
fn validate_codex_code_mode_candidate_v1(
    descriptor: &CodexNativeLocalCodeModeDescriptorV1,
    projection: &ToolProjectionMap,
    registry: FrozenToolRegistry,
    configuration: &CompiledProviderConfiguration,
    minimal: bool,
) -> Result<(), ToolRuntimeError> {
    if !projection.is_for_registry(&registry) {
        return Err(ToolRuntimeError::new(
            ToolRuntimeErrorKind::Registration,
            "selected Code Mode outer projection belongs to a foreign registry snapshot",
        ));
    }
    if !descriptor.outer_projection().is_for_registry(&registry) {
        return Err(ToolRuntimeError::new(
            ToolRuntimeErrorKind::Registration,
            "Code Mode descriptor outer projection belongs to a foreign registry snapshot",
        ));
    }
    if !descriptor.nested_projection().is_for_registry(&registry) {
        return Err(ToolRuntimeError::new(
            ToolRuntimeErrorKind::Registration,
            "Code Mode descriptor nested projection belongs to a foreign registry snapshot",
        ));
    }

    let effective = configuration.effective();
    let model = effective.descriptor();
    let family = model.model_family().ok_or_else(|| {
        ToolRuntimeError::new(
            ToolRuntimeErrorKind::Registration,
            "Code Mode provider configuration has no exact model family",
        )
    })?;
    let expected_target = ProviderModelProjectionIdentityV2::new(
        model.integration_class(),
        model.protocol().as_str(),
        model.route().as_str(),
        model.profile().as_str(),
        family.as_str(),
        model.requested_model().as_str(),
        effective.effective_model().as_str(),
        configuration.effective_identity(),
        plakous_tools::PROCESS_TOOL_PROJECTION_REVISION,
    )?;
    // The OpenAI API route wears the Codex pack; its profiles and model
    // families are that route's own enrollment.
    let exact_codex_target = (expected_target.integration_class() != IntegrationClass::Custom
        && expected_target.protocol() == CODEX_PROJECTION_PROTOCOL
        && expected_target.route() == CODEX_PROJECTION_ROUTE
        && expected_target.integration_profile() == CODEX_PROJECTION_PROFILE
        && expected_target.family() == CODEX_NATIVE_CODE_MODE_FAMILY_V1)
        || (expected_target.integration_class() == IntegrationClass::Standard
            && expected_target.protocol() == OPENAI_PLATFORM_PROJECTION_PROTOCOL
            && expected_target.route() == OPENAI_PLATFORM_PROJECTION_ROUTE);
    // One additional enrolled target under the harness feature; the condition
    // above is unchanged and still the only way a Codex target qualifies.
    #[cfg(feature = "scripted-code-mode-enrolment")]
    let exact_codex_target = exact_codex_target || is_scripted_code_mode_target(&expected_target);
    if !exact_codex_target {
        return Err(ToolRuntimeError::new(
            ToolRuntimeErrorKind::Registration,
            "provider configuration is not an enrolled exact Codex Code Mode target",
        ));
    }
    if descriptor.outer_projection().target() != &expected_target
        || descriptor.nested_projection().target() != &expected_target
    {
        return Err(ToolRuntimeError::new(
            ToolRuntimeErrorKind::Registration,
            "Code Mode descriptor is foreign to the effective provider configuration",
        ));
    }
    if projection.target() != &expected_target {
        return Err(ToolRuntimeError::new(
            ToolRuntimeErrorKind::Registration,
            "selected Code Mode outer projection is foreign to the effective provider configuration",
        ));
    }

    let configured = &effective.behavior().tools;
    if configured.len() != 2
        || configured.iter().any(|tool| {
            tool.execution() != ToolExecutionDisposition::Local
                || !matches!(tool.definition().name().as_str(), "exec" | "wait")
                || registry.definition(tool.definition().name()) != Some(tool.definition())
        })
        || !configured
            .iter()
            .any(|tool| tool.definition().name().as_str() == CODEX_NATIVE_CODE_MODE_EXEC_NAME_V1)
        || !configured
            .iter()
            .any(|tool| tool.definition().name().as_str() == CODEX_NATIVE_CODE_MODE_WAIT_NAME_V1)
    {
        return Err(ToolRuntimeError::new(
            ToolRuntimeErrorKind::Registration,
            "Code Mode provider configuration does not own the exact local exec/wait markers",
        ));
    }

    let expected = if minimal {
        let catalog = descriptor
            .nested_tools()
            .iter()
            .map(|tool| tool.exposed_name().clone())
            .collect::<Vec<_>>();
        CodexMinimalLocalCodeModeV1::compile_for_catalog(
            descriptor.job_config(),
            registry,
            expected_target,
            &catalog,
        )?
        .0
    } else {
        CodexNativeLocalCodeModeV1::compile(descriptor.job_config(), registry, expected_target)?
    };
    if descriptor.identity() != expected.identity()
        || descriptor.outer_projection().digest() != expected.outer_projection().digest()
        || descriptor.nested_projection().digest() != expected.nested_projection().digest()
        || projection.digest() != expected.outer_projection().digest()
    {
        return Err(ToolRuntimeError::new(
            ToolRuntimeErrorKind::Registration,
            "Code Mode descriptor or selected outer projection is not the exact catalog candidate",
        ));
    }
    Ok(())
}

/// One ordered function entry in the exact seven-tool nested catalog.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CodexCodeModeNestedToolV1 {
    exposed_name: ToolName,
    description: String,
    input_schema: Option<PortableToolSchemaV2>,
    string_input: bool,
    projection_identity: Digest,
}

impl CodexCodeModeNestedToolV1 {
    /// JavaScript-global and canonical invocation name.
    #[must_use]
    pub const fn exposed_name(&self) -> &ToolName {
        &self.exposed_name
    }

    /// Exact model-visible nested description.
    #[must_use]
    pub fn description(&self) -> &str {
        &self.description
    }

    /// Exact structured-function input schema, absent for freeform nested tools.
    #[must_use]
    pub const fn input_schema(&self) -> Option<&PortableToolSchemaV2> {
        self.input_schema.as_ref()
    }

    /// Whether nested TypeScript uses `(input: string)` instead of structured args.
    #[must_use]
    pub const fn string_input(&self) -> bool {
        self.string_input
    }

    /// JSON Schema for neutral local-composition attachment (string for freeform tools).
    #[must_use]
    pub fn composition_input_schema_value(&self) -> Value {
        if self.string_input {
            json!({"type": "string"})
        } else {
            self.input_schema.as_ref().map_or_else(
                || json!({"type": "object"}),
                |schema| schema.document().value().as_ref().clone(),
            )
        }
    }

    /// Identity of the lowering/executor-bound projection entry.
    #[must_use]
    pub const fn projection_identity(&self) -> Digest {
        self.projection_identity
    }
}

fn extra_nested_names(descriptor: &CodexNativeLocalCodeModeDescriptorV1) -> Vec<ToolName> {
    descriptor
        .nested_tools
        .iter()
        .filter(|tool| !MINIMAL_CORE_NESTED_NAMES.contains(&tool.exposed_name.as_str()))
        .map(|tool| tool.exposed_name.clone())
        .collect()
}

fn nested_projection(
    job_config: plakous_tools::JobConfig,
    registry: FrozenToolRegistry,
    target: ProviderModelProjectionIdentityV2,
    include_extensions: bool,
    extra_nested: &[&str],
) -> Result<ToolProjectionMap, ToolRuntimeError> {
    let process = ProcessToolProjectionKind::CodexUnifiedExec.projection_map(
        job_config,
        registry.clone(),
        target.clone(),
    )?;
    let mut entries = process.entries().to_vec();
    let core = [APPLY_PATCH_CAPABILITY_NAME, UPDATE_PLAN_CAPABILITY_NAME];
    let extended = [
        READ_FILE_CAPABILITY_NAME,
        SEARCH_REPOSITORY_CAPABILITY_NAME,
        INSPECT_IMAGE_CAPABILITY_NAME,
    ];
    for name in core
        .into_iter()
        .chain(include_extensions.then_some(extended).into_iter().flatten())
    {
        if name == APPLY_PATCH_CAPABILITY_NAME {
            // The files pack owns how a Codex costume sees apply_patch; a
            // nested cell only raises it differently.
            entries.push(resolve_entry(
                &registry,
                &target,
                &codex_patch_envelope_entry()?.with_raising(ToolProjectionRaisingV2::RawUtf8V1),
            )?);
            continue;
        }
        let lowering = ToolProjectionLoweringV2::CanonicalPassthrough;
        let definition = canonical_definition(&registry, name)?;
        let ToolInputContract::Structured(schema) = definition.input() else {
            return Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::Registration,
                "nested coding capability is not structured",
            ));
        };
        entries.push(capability_entry(
            &registry,
            &target,
            name,
            definition.description(),
            schema.clone(),
            lowering,
        )?);
    }
    for name in extra_nested {
        if entries
            .iter()
            .any(|entry| entry.exposed_name().as_str() == *name)
        {
            return Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::Registration,
                "product Code Mode extension collides with an already projected nested capability",
            ));
        }
        let definition = canonical_definition(&registry, name)?;
        let ToolInputContract::Structured(schema) = definition.input() else {
            return Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::Registration,
                "nested coding capability is not structured",
            ));
        };
        entries.push(capability_entry(
            &registry,
            &target,
            name,
            definition.description(),
            schema.clone(),
            ToolProjectionLoweringV2::CanonicalPassthrough,
        )?);
    }
    plakous_tools::internal::projection_map(registry, target, entries)
}

fn capability_entry(
    registry: &FrozenToolRegistry,
    target: &ProviderModelProjectionIdentityV2,
    canonical_name: &str,
    description: &str,
    input_schema: PortableToolSchemaV2,
    lowering: ToolProjectionLoweringV2,
) -> Result<ToolProjectionExposureV2, ToolRuntimeError> {
    let definition = canonical_definition(registry, canonical_name)?;
    let identity = registry
        .identity(definition.name())
        .cloned()
        .ok_or_else(|| {
            ToolRuntimeError::new(
                ToolRuntimeErrorKind::Registration,
                "nested coding capability identity is absent",
            )
        })?;
    let operation = if definition.effect() == ToolEffect::ReadOnly {
        CanonicalProcessOperationV2::Poll
    } else {
        CanonicalProcessOperationV2::Start
    };
    plakous_tools::internal::projection_exposure(
        [(operation, identity)],
        canonical_name,
        description,
        input_schema,
        empty_defaults()?,
        lowering,
        nested_output_schema()?,
        // Nested lineage raises the raw text (G1-2): the descriptions promise
        // strings and the cell must not have to unwrap `{"result": ...}`.
        ToolProjectionRaisingV2::RawUtf8V1,
        target.clone(),
    )
}

fn outer_projection(
    registry: FrozenToolRegistry,
    target: ProviderModelProjectionIdentityV2,
    exec_description: String,
    wait_description: String,
) -> Result<ToolProjectionMap, ToolRuntimeError> {
    let exec = registered_identity(&registry, CODEX_NATIVE_CODE_MODE_EXEC_NAME_V1)?;
    let wait = registered_identity(&registry, CODEX_NATIVE_CODE_MODE_WAIT_NAME_V1)?;
    let exec_entry = plakous_tools::internal::projection_exposure_custom(
        [(CanonicalProcessOperationV2::Poll, exec)],
        CODEX_NATIVE_CODE_MODE_EXEC_NAME_V1,
        exec_description,
        Utf8TextContractV1::new(CODE_MODE_EXEC_MAX_BYTES_V1)
            .map_err(|error| definition_error("Code Mode exec bound is invalid", &error))?,
        ProjectedCustomToolFormatV2::lark(CODEX_NATIVE_CODE_MODE_EXEC_GRAMMAR_V1)
            .map_err(|error| definition_error("Code Mode exec grammar is invalid", &error))?,
        empty_defaults()?,
        ToolProjectionLoweringV2::CanonicalPassthrough,
        generic_output_schema()?,
        ToolProjectionRaisingV2::ModelContentIdentityV1,
        target.clone(),
    )?;
    let wait_entry = plakous_tools::internal::projection_exposure(
        [(CanonicalProcessOperationV2::Poll, wait)],
        CODEX_NATIVE_CODE_MODE_WAIT_NAME_V1,
        wait_description,
        PortableToolSchemaV2::new(codex_code_mode_wait_schema())
            .map_err(|error| definition_error("Code Mode wait schema is invalid", &error))?,
        empty_defaults()?,
        ToolProjectionLoweringV2::CanonicalPassthrough,
        generic_output_schema()?,
        ToolProjectionRaisingV2::ModelContentIdentityV1,
        target.clone(),
    )?;
    plakous_tools::internal::projection_map(registry, target, [exec_entry, wait_entry])
}

fn ordered_nested_catalog(
    projection: &ToolProjectionMap,
    nested_names: &[&str],
) -> Result<Vec<CodexCodeModeNestedToolV1>, ToolRuntimeError> {
    let view = projection.compile_request_view_v2()?;
    nested_names
        .iter()
        .map(|name| {
            let definition = view
                .definitions()
                .iter()
                .find(|definition| definition.exposed_name().as_str() == *name)
                .ok_or_else(|| {
                    ToolRuntimeError::new(
                        ToolRuntimeErrorKind::Registration,
                        "exact Code Mode nested tool is absent",
                    )
                })?;
            let (input_schema, string_input) = match definition.presentation() {
                ProjectedToolPresentationV2::CustomUtf8 { .. } => (None, true),
                ProjectedToolPresentationV2::StructuredFunction { .. } => {
                    let schema = definition.input_schema().ok_or_else(|| {
                        ToolRuntimeError::new(
                            ToolRuntimeErrorKind::Registration,
                            "Code Mode nested tool must be structured or custom UTF-8",
                        )
                    })?;
                    (Some(schema.clone()), false)
                }
            };
            Ok(CodexCodeModeNestedToolV1 {
                exposed_name: definition.exposed_name().clone(),
                description: definition.description().to_owned(),
                input_schema,
                string_input,
                projection_identity: definition.source_identity(),
            })
        })
        .collect()
}

/// Stated once per route: every nested declaration below is a member of the
/// one injected `declare const tools` object, so each entry carries its own
/// signature and not another copy of the wrapper.
const EXEC_DECLARATION_HEADER: &str =
    "Nested tool declarations, all members of `declare const tools`:";

// The local composition bridge delivers a single ToolResultPart::Text as a
// string, including the JSON-encoded process result. Do not advertise fields
// on the unparsed value: `result.output` silently evaluates to undefined.
const PROCESS_TOOL_RESULT_TYPE: &str = "Promise<string>";

fn build_exec_description(tools: &[CodexCodeModeNestedToolV1], exact_reference: bool) -> String {
    let template = if exact_reference {
        EXACT_REFERENCE_EXEC_DESCRIPTION_TEMPLATE
    } else {
        MINIMAL_EXEC_DESCRIPTION_TEMPLATE
    };
    let nested = tools
        .iter()
        .map(|tool| {
            let name = tool.exposed_name.as_str();
            let signature = if tool.string_input {
                format!("{name}(input: string)")
            } else {
                let input = tool.input_schema.as_ref().map_or_else(
                    || "unknown".to_owned(),
                    |schema| render_schema_type(schema.document().value()),
                );
                format!("{name}(args: {input})")
            };
            let return_type = if matches!(name, "exec_command" | "write_stdin") {
                PROCESS_TOOL_RESULT_TYPE
            } else {
                "Promise<unknown>"
            };
            format!(
                "### `{name}`\n{}\n\n```ts\n{signature}: {return_type};\n```",
                tool.description
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    format!("{template}\n\n{EXEC_DECLARATION_HEADER}\n\n{nested}")
}

pub(crate) fn render_schema_type(schema: &Value) -> String {
    let Some(object) = schema.as_object() else {
        return "unknown".to_owned();
    };
    if let Some(values) = object.get("enum").and_then(Value::as_array) {
        return values
            .iter()
            .map(|value| serde_json::to_string(value).unwrap_or_else(|_| "unknown".to_owned()))
            .collect::<Vec<_>>()
            .join(" | ");
    }
    match object.get("type").and_then(Value::as_str) {
        Some("string") => "string".to_owned(),
        Some("integer" | "number") => "number".to_owned(),
        Some("boolean") => "boolean".to_owned(),
        Some("null") => "null".to_owned(),
        Some("array") => object.get("items").map_or_else(
            || "unknown[]".to_owned(),
            |items| format!("Array<{}>", render_schema_type(items)),
        ),
        Some("object") | None
            if object.contains_key("properties") || object.contains_key("additionalProperties") =>
        {
            render_object_type(object)
        }
        _ => "unknown".to_owned(),
    }
}

fn render_object_type(object: &serde_json::Map<String, Value>) -> String {
    let empty = serde_json::Map::new();
    let properties = object
        .get("properties")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    let has_descriptions = properties.values().any(|schema| {
        schema
            .as_object()
            .and_then(|property| property.get("description"))
            .is_some()
    });
    if has_descriptions {
        render_commented_object_type(object)
    } else {
        render_inline_object_type(object)
    }
}

fn render_inline_object_type(object: &serde_json::Map<String, Value>) -> String {
    let empty = serde_json::Map::new();
    let properties = object
        .get("properties")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    let required = object
        .get("required")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .collect::<BTreeSet<_>>()
        })
        .unwrap_or_default();
    let mut properties = properties.iter().collect::<Vec<_>>();
    properties.sort_unstable_by_key(|(name, _)| *name);
    let fields = properties
        .into_iter()
        .map(|(name, schema)| {
            let optional = if required.contains(name.as_str()) {
                ""
            } else {
                "?"
            };
            let rendered_name = if normalize_javascript_identifier(name) == *name {
                name.clone()
            } else {
                serde_json::to_string(name).unwrap_or_else(|_| format!("\"{name}\""))
            };
            format!("{rendered_name}{optional}: {};", render_schema_type(schema))
        })
        .collect::<Vec<_>>();
    if fields.is_empty() {
        "{}".to_owned()
    } else {
        format!("{{ {} }}", fields.join(" "))
    }
}

fn render_commented_object_type(object: &serde_json::Map<String, Value>) -> String {
    let empty = serde_json::Map::new();
    let properties = object
        .get("properties")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    let required = object
        .get("required")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .collect::<BTreeSet<_>>()
        })
        .unwrap_or_default();
    let mut properties = properties.iter().collect::<Vec<_>>();
    properties.sort_unstable_by_key(|(name, _)| *name);
    let fields = properties
        .into_iter()
        .map(|(name, schema)| {
            let optional = if required.contains(name.as_str()) {
                ""
            } else {
                "?"
            };
            let rendered_name = if normalize_javascript_identifier(name) == *name {
                name.clone()
            } else {
                serde_json::to_string(name).unwrap_or_else(|_| format!("\"{name}\""))
            };
            let comment = schema
                .as_object()
                .and_then(|property| property.get("description"))
                .and_then(Value::as_str)
                .map_or_else(
                    || "  ".to_owned(),
                    |description| format!("  // {description}\n  "),
                );
            format!(
                "{comment}{rendered_name}{optional}: {};",
                render_schema_type(schema)
            )
        })
        .collect::<Vec<_>>();
    if fields.is_empty() {
        "{}".to_owned()
    } else {
        format!("{{\n{}\n}}", fields.join("\n"))
    }
}

fn validate_nested_catalog(
    tools: &[CodexCodeModeNestedToolV1],
    nested_names: &[&str],
) -> Result<(), ToolRuntimeError> {
    if tools.len() != nested_names.len() {
        return Err(ToolRuntimeError::new(
            ToolRuntimeErrorKind::Registration,
            "Code Mode nested catalog membership is not exact",
        ));
    }
    let mut normalized = BTreeSet::new();
    for (tool, expected) in tools.iter().zip(nested_names) {
        if tool.exposed_name.as_str() != *expected {
            return Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::Registration,
                "Code Mode nested catalog order invariant failed",
            ));
        }
        let javascript_name = normalize_javascript_identifier(tool.exposed_name.as_str());
        if !normalized.insert(javascript_name) {
            return Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::Registration,
                "Code Mode nested JavaScript tool-name collision",
            ));
        }
    }
    Ok(())
}

/// The JavaScript identifier a nested tool is reachable as on `tools`:
/// Codex Code Mode's rule, every character outside `[A-Za-z0-9_$]` (and a
/// leading digit) mapped to `_` (`mcp__srv__get-sum` -> `mcp__srv__get_sum`).
#[must_use]
pub fn normalize_javascript_identifier(name: &str) -> String {
    let mut normalized = String::with_capacity(name.len());
    for (index, character) in name.chars().enumerate() {
        let valid = if index == 0 {
            character == '_' || character == '$' || character.is_ascii_alphabetic()
        } else {
            character == '_' || character == '$' || character.is_ascii_alphanumeric()
        };
        normalized.push(if valid { character } else { '_' });
    }
    if normalized.is_empty() {
        normalized.push('_');
    }
    normalized
}

fn descriptor_identity(
    outer: &ToolProjectionMap,
    nested: &ToolProjectionMap,
    nested_tools: &[CodexCodeModeNestedToolV1],
    exec_description: &str,
    wait_description: &str,
    identity_label: &[u8],
) -> Result<Digest, ToolRuntimeError> {
    let nested_bytes = serde_json::to_vec(nested_tools).map_err(|error| {
        definition_error("Code Mode nested catalog could not be encoded", &error)
    })?;
    Digest::sha256(
        DigestDomain::ToolDefinition,
        CODE_MODE_DESCRIPTOR_DIGEST_VERSION,
        &[
            identity_label,
            CODEX_NATIVE_CODE_MODE_RELEASE_V1.as_bytes(),
            CODEX_NATIVE_CODE_MODE_COMMIT_V1.as_bytes(),
            CODEX_NATIVE_CODE_MODE_ENGINE_REVISION_V1.as_bytes(),
            &CODEX_NATIVE_CODE_MODE_DEFAULT_YIELD_MS_V1.to_be_bytes(),
            &CODEX_NATIVE_CODE_MODE_DEFAULT_OUTPUT_TOKENS_V1.to_be_bytes(),
            CODEX_NATIVE_CODE_MODE_EXEC_GRAMMAR_V1.as_bytes(),
            exec_description.as_bytes(),
            wait_description.as_bytes(),
            outer.digest().bytes(),
            nested.digest().bytes(),
            &nested_bytes,
        ],
    )
    .map_err(|error| definition_error("Code Mode descriptor identity could not be built", &error))
}

fn canonical_definition<'a>(
    registry: &'a FrozenToolRegistry,
    name: &str,
) -> Result<&'a ToolDefinition, ToolRuntimeError> {
    let name = ToolName::new(name.to_owned())
        .map_err(|error| definition_error("canonical Code Mode tool name is invalid", &error))?;
    registry.definition(&name).ok_or_else(|| {
        ToolRuntimeError::new(
            ToolRuntimeErrorKind::Registration,
            "required canonical Code Mode capability is not registered",
        )
    })
}

fn registered_identity(
    registry: &FrozenToolRegistry,
    name: &str,
) -> Result<plakous_tools::RegisteredToolIdentity, ToolRuntimeError> {
    let definition = canonical_definition(registry, name)?;
    registry
        .identity(definition.name())
        .cloned()
        .ok_or_else(|| {
            ToolRuntimeError::new(
                ToolRuntimeErrorKind::Registration,
                "Code Mode marker identity is absent",
            )
        })
}

fn empty_defaults() -> Result<SemanticJsonV1, ToolRuntimeError> {
    SemanticJsonV1::from_value(json!({}))
        .map_err(|error| definition_error("Code Mode defaults are invalid", &error))
}

fn nested_output_schema() -> Result<PortableToolSchemaV2, ToolRuntimeError> {
    PortableToolSchemaV2::new(object_schema(json!({}), &[]))
        .map_err(|error| definition_error("Code Mode nested output schema is invalid", &error))
}

fn generic_output_schema() -> Result<PortableToolSchemaV2, ToolRuntimeError> {
    PortableToolSchemaV2::new(object_schema(json!({"result": {"type": "string"}}), &[]))
        .map_err(|error| definition_error("Code Mode output schema is invalid", &error))
}

fn definition_error(
    context: &'static str,
    error: &(dyn std::error::Error + 'static),
) -> ToolRuntimeError {
    ToolRuntimeError::from_sensitive_source(ToolRuntimeErrorKind::InvalidDefinition, context, error)
}
