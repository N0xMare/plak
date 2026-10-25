//! Exact Codex OAuth `CodeModeOnly` presentations and nested coding catalogs.
//!
//! The frozen seven-tool reference remains exact. The composition adds the
//! separately identified four-tool minimum-optimal default. Product-owned
//! nested-eligible extensions may append to the four-tool catalog through an
//! explicit compile and rotate that catalog's identity; `NestedEligible` alone
//! never implies exposure. The seven-tool reference does not accept extras.

use super::super::compile::{NestedProjection, compile_cell};
use super::CellPresentation;
#[cfg(test)]
use plakous_model::DigestDomain;
use plakous_model::{
    CompiledProviderConfiguration, Digest, IntegrationClass, ToolExecutionDisposition, ToolName,
};
use plakous_tools::{
    CODEX_NATIVE_CODE_MODE_DRIVER_MARKER_EXECUTOR_ID_V1, CODEX_NATIVE_CODE_MODE_EXEC_NAME_V1,
    CODEX_NATIVE_CODE_MODE_WAIT_NAME_V1, FrozenToolRegistry, PortableToolSchemaV2,
    ProviderModelProjectionIdentityV2, ToolExecutorId, ToolProjectionMap, ToolRegistryBuilder,
    ToolRuntimeError, ToolRuntimeErrorKind, register_codex_code_mode_markers_into,
};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::sync::Arc;

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

pub(super) const WAIT_DESCRIPTION: &str = "Waits on a yielded `exec` cell and returns new output or completion.\n- Use `wait` only after `exec` returns `Script running with cell ID ...`.\n- `cell_id` identifies the running `exec` cell to resume.\n- `yield_time_ms` controls how long to wait for more output before yielding again. Defaults to 10000 ms; clamped to at most 600000 ms.\n- `max_tokens` limits how much new output this wait call returns. Defaults to 10000 tokens.\n- `terminate: true` stops the running cell; false or omitted waits for output.\n- `wait` returns only the new output since the last yield, or the final completion or termination result for that cell.\n- If the cell is still running, `wait` may yield again with the same `cell_id`.\n- If the cell has already finished, `wait` returns the completed result and closes the cell.";
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
            "plakous-tools/codex-native-local-code-mode-v1",
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
            "plakous-tools/codex-minimal-local-code-mode-v1"
        } else {
            "plakous-tools/codex-minimal-local-code-mode-v1.subagents.v1"
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
    pub(in crate::cell) job_config: plakous_tools::JobConfig,
    pub(in crate::cell) outer_projection: ToolProjectionMap,
    pub(in crate::cell) nested_projection: ToolProjectionMap,
    pub(in crate::cell) nested_tools: Arc<[CodexCodeModeNestedToolV1]>,
    pub(in crate::cell) exec_description: Arc<str>,
    pub(in crate::cell) wait_description: Arc<str>,
    pub(in crate::cell) identity: Digest,
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
    pub(in crate::cell) exposed_name: ToolName,
    pub(in crate::cell) description: String,
    pub(in crate::cell) input_schema: Option<PortableToolSchemaV2>,
    pub(in crate::cell) string_input: bool,
    pub(in crate::cell) projection_identity: Digest,
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

fn definition_error(
    context: &'static str,
    error: &(dyn std::error::Error + 'static),
) -> ToolRuntimeError {
    ToolRuntimeError::from_sensitive_source(ToolRuntimeErrorKind::InvalidDefinition, context, error)
}

fn compile_code_mode_descriptor(
    job_config: plakous_tools::JobConfig,
    registry: FrozenToolRegistry,
    target: ProviderModelProjectionIdentityV2,
    nested_names: &[&str],
    include_extensions: bool,
    extra_nested: &[&str],
    profile: &'static str,
) -> Result<CodexNativeLocalCodeModeDescriptorV1, ToolRuntimeError> {
    let names = nested_names
        .iter()
        .map(|name| ToolName::new(*name))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| definition_error("nested Code Mode name is invalid", &error))?;
    let presentation = CellPresentation::codex(include_extensions, profile);
    let cell = compile_cell(
        &presentation,
        job_config,
        registry,
        target,
        &names,
        NestedProjection::Codex {
            include_extensions,
            extra_nested,
        },
    )?;
    Ok(CodexNativeLocalCodeModeDescriptorV1 {
        job_config,
        nested_tools: cell.api.codex_tools()?.into(),
        outer_projection: cell.outer_projection,
        nested_projection: cell.nested_projection,
        exec_description: cell.execute_text.into(),
        wait_description: presentation.messages.wait.into(),
        identity: cell.identity,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_description_string_members_follow_runtime_collision_rules()
    -> Result<(), Box<dyn std::error::Error>> {
        let tool = |name: &str| -> Result<_, Box<dyn std::error::Error>> {
            Ok(CodexCodeModeNestedToolV1 {
                exposed_name: ToolName::new(name)?,
                description: "A string tool.".to_owned(),
                input_schema: None,
                string_input: true,
                projection_identity: Digest::sha256(
                    DigestDomain::ToolDefinition,
                    1,
                    &[name.as_bytes()],
                )?,
            })
        };
        for exact_reference in [false, true] {
            let tools = [tool("mcp__srv__get-sum")?];
            let api = super::super::super::host_api::HostApi::codex_catalog(&tools);
            let style = if exact_reference {
                super::super::CatalogStyle::CodexExact
            } else {
                super::super::CatalogStyle::CodexMinimal
            };
            let description = super::super::super::render_ts::render_ts(&api, style);
            assert!(description.contains("mcp__srv__get_sum(input: string): Promise<unknown>;"));
            assert!(description.contains("(call `tools.mcp__srv__get_sum`)"));

            let tools = [tool("mcp__srv__get-sum")?, tool("mcp__srv__get_sum")?];
            let api = super::super::super::host_api::HostApi::codex_catalog(&tools);
            let style = if exact_reference {
                super::super::CatalogStyle::CodexExact
            } else {
                super::super::CatalogStyle::CodexMinimal
            };
            let description = super::super::super::render_ts::render_ts(&api, style);
            assert!(
                description.contains("\"mcp__srv__get-sum\"(input: string): Promise<unknown>;")
            );
            assert!(description.contains("(call `tools[\"mcp__srv__get-sum\"]`)"));
            assert!(description.contains("### `mcp__srv__get_sum`\n"));
        }
        Ok(())
    }
}
