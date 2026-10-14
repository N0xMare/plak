//! Frozen provider-facing aliases over exact registered tool identities.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use plakous_model::{
    CompiledToolProjectionV2, Digest, DigestDomain, EffectiveProviderConfigurationId,
    IntegrationClass, ProjectedCustomToolFormatV2, ProjectedToolDefinitionV2,
    ProjectedToolPresentationV2, SemanticJsonV1, ToolDefinition, ToolEffect, ToolInputContract,
    ToolName, ToolProjectionTargetV2, ToolResultPart, Utf8TextContractV1,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::capabilities::{CLAUDE_BASH_OBSERVATION_BYTES, PROCESS_OBSERVATION_BYTES};
use crate::{
    ExecutionEnvironment, FrozenToolRegistry, PortableToolSchemaV2, RegisteredToolIdentity,
    ToolInvocation, ToolInvocationRequest, ToolOutcome, ToolRuntimeError, ToolRuntimeErrorKind,
};

const REGISTRY_IDENTITY_DIGEST_VERSION: u32 = 9;
const EXPOSURE_MAP_DIGEST_VERSION: u32 = 8;
const PROJECTION_MAP_DIGEST_VERSION: u32 = 4;

/// Names only absent top-level keys from the already bounded, model-visible
/// schema. Values from the rejected call are never copied into the error.
fn missing_required_fields(schema: &PortableToolSchemaV2, raw: &str) -> Option<String> {
    let instance: Value = serde_json::from_str(raw).ok()?;
    let instance = instance.as_object()?;
    let document = schema.document().value().as_object()?;
    let missing = document
        .get("required")?
        .as_array()?
        .iter()
        .filter_map(Value::as_str)
        .filter(|field| !instance.contains_key(*field))
        .collect::<Vec<_>>();
    (!missing.is_empty()).then(|| missing.join(", "))
}

/// Closed lowering programs enrolled by the V2 projection catalog.
///
/// This is data interpreted by Plakous, not a public projection plugin seam.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolProjectionLoweringV2 {
    /// Codex `exec_command` into the canonical process-session operation.
    CodexExecCommand,
    /// Codex `write_stdin` into the canonical process-session operation.
    CodexWriteStdin,
    /// Grok Build `run_terminal_command` into the canonical process-session operation.
    GrokRunTerminalCommand,
    /// Grok Build process-only snapshot or multi-process await.
    GrokGetCommandOutput,
    /// Grok Build stop of one exact process job.
    GrokKillCommand,
    /// Codex-envelope `apply_patch` into the canonical mutator.
    CodexApplyPatch,
    /// Grok Build `read_file` into canonical read.
    GrokReadFile,
    /// Grok Build `grep` into canonical search.
    GrokGrep,
    /// Grok Build `search_replace` into canonical mutate.
    GrokSearchReplace,
    /// Grok Build `write` into canonical file mutation.
    GrokWrite,
    /// Grok Build `list_dir` into canonical repository listing.
    GrokListDir,
    /// Claude OAuth `Read` into canonical read.
    ClaudeReadFile,
    /// Claude OAuth `Bash` into canonical process start.
    ClaudeBash,
    /// Claude OAuth `TaskStop` into canonical process terminate.
    ClaudeTaskStop,
    /// Claude OAuth `Write` into canonical file create or overwrite via `apply_patch`.
    ClaudeWrite,
    /// Claude OAuth `Edit` into canonical mutate; empty `old_string` is refused.
    ClaudeEdit,
    /// Pi `read` into canonical read.
    PiRead,
    /// Pi `bash` into canonical process start; `timeout` 0 starts in the background.
    PiBash,
    /// Pi `edit` into one canonical multi-edit mutation; empty `oldText` is refused.
    PiEdit,
    /// Pi `write` into canonical file create or overwrite.
    PiWrite,
    /// Pi `wait` into a canonical process await that wakes on the first terminal.
    PiWait,
    /// Pi `stop` into canonical process terminate.
    PiStop,
    /// Projected arguments already match the bound canonical schema.
    CanonicalPassthrough,
}

impl ToolProjectionLoweringV2 {
    const fn operation(self) -> &'static str {
        match self {
            Self::CodexExecCommand => "exec_command-to-start-or-poll",
            Self::CodexWriteStdin => "write_stdin-to-write",
            Self::GrokRunTerminalCommand => "grok-block-until-to-promotable-start-v2",
            Self::GrokGetCommandOutput => "grok-task-ids-to-process-await-v1",
            Self::GrokKillCommand => "grok-task-id-to-process-stop-v1",
            Self::CodexApplyPatch => "codex-envelope-to-apply-patch",
            Self::GrokReadFile => "grok-read-file-to-read",
            Self::GrokGrep => "grok-grep-to-search",
            Self::GrokSearchReplace => "grok-search-replace-to-mutate",
            Self::GrokWrite => "grok-write-to-mutate",
            Self::GrokListDir => "grok-list-dir-to-search",
            Self::ClaudeReadFile => "claude-read-to-read",
            Self::ClaudeBash => "claude-bash-to-joined-start",
            Self::ClaudeTaskStop => "claude-task-stop-to-terminate-v2",
            Self::ClaudeWrite => "claude-write-to-create-or-overwrite",
            Self::ClaudeEdit => "claude-edit-to-mutate",
            Self::PiRead => "pi-read-to-read",
            Self::PiBash => "pi-bash-to-joined-start",
            Self::PiEdit => "pi-edits-to-mutate",
            Self::PiWrite => "pi-write-to-create-or-overwrite",
            Self::PiWait => "pi-ids-to-first-terminal-await",
            Self::PiStop => "pi-id-to-terminate",
            Self::CanonicalPassthrough => "canonical-passthrough",
        }
    }
}

/// Closed deterministic raising programs enrolled by the V2 catalog.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolProjectionRaisingV2 {
    /// The canonical process executor already emits the shared structured result.
    ProcessResult,
    /// Canonical tool text is already the model-visible result.
    Identity,
    /// Wrap canonical text in the declared `{ "result": string }` object.
    TextResultObjectV1,
    /// Canonical UTF-8 text is already the nested-cell-visible result.
    RawUtf8V1,
    /// Preserve bounded canonical model-content parts for composition markers.
    ModelContentIdentityV1,
}

impl ToolProjectionRaisingV2 {
    const fn operation(self) -> &'static str {
        match self {
            Self::ProcessResult => "process_result",
            Self::Identity => "validated_identity_v2",
            Self::TextResultObjectV1 => "text_result_object_v1",
            Self::RawUtf8V1 => "raw_utf8_v1",
            Self::ModelContentIdentityV1 => "model_content_identity_v1",
        }
    }
}

/// Closed canonical operation set selected by validated process lowering.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CanonicalProcessOperationV2 {
    /// Start one owned joined process session.
    Start,
    /// Write bytes or close stdin on one exact session.
    Write,
    /// Read retained output from one exact cursor without changing process state.
    Poll,
    /// Terminate and join one exact owned session.
    Terminate,
}

/// Exact provider/profile/family/model/configuration candidate identity for one projection set.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderModelProjectionIdentityV2 {
    integration_class: IntegrationClass,
    protocol: String,
    route: String,
    integration_profile: String,
    family: String,
    requested_model: String,
    effective_model: String,
    effective_provider_configuration: EffectiveProviderConfigurationId,
    projection_revision: u32,
}

impl ProviderModelProjectionIdentityV2 {
    /// Constructs one exact candidate tuple. Requested and effective models must agree.
    ///
    /// # Errors
    ///
    /// Rejects non-portable, zero-revision, or model-drifted tuples.
    #[allow(
        clippy::too_many_arguments,
        reason = "the candidate projection target is one closed exact authority tuple"
    )]
    pub fn new(
        integration_class: IntegrationClass,
        protocol: impl Into<String>,
        route: impl Into<String>,
        integration_profile: impl Into<String>,
        family: impl Into<String>,
        requested_model: impl Into<String>,
        effective_model: impl Into<String>,
        effective_provider_configuration: EffectiveProviderConfigurationId,
        projection_revision: u32,
    ) -> Result<Self, ToolRuntimeError> {
        let value = Self {
            integration_class,
            protocol: protocol.into(),
            route: route.into(),
            integration_profile: integration_profile.into(),
            family: family.into(),
            requested_model: requested_model.into(),
            effective_model: effective_model.into(),
            effective_provider_configuration,
            projection_revision,
        };
        let fields = [
            value.protocol.as_str(),
            value.route.as_str(),
            value.integration_profile.as_str(),
            value.family.as_str(),
            value.requested_model.as_str(),
            value.effective_model.as_str(),
        ];
        if value.projection_revision == 0
            || value.requested_model != value.effective_model
            || fields.iter().any(|field| {
                field.is_empty()
                    || field.len() > 256
                    || !field.bytes().all(|byte| byte.is_ascii_graphic())
            })
        {
            return Err(registration_error(
                "provider/model projection identity is invalid",
            ));
        }
        Ok(value)
    }

    /// Closed standard/custom integration class.
    #[must_use]
    pub const fn integration_class(&self) -> IntegrationClass {
        self.integration_class
    }

    /// Exact protocol identity.
    #[must_use]
    pub fn protocol(&self) -> &str {
        &self.protocol
    }
    /// Exact provider route identity.
    #[must_use]
    pub fn route(&self) -> &str {
        &self.route
    }
    /// Exact integration-profile identity.
    #[must_use]
    pub fn integration_profile(&self) -> &str {
        &self.integration_profile
    }
    /// Exact protocol-family identity.
    #[must_use]
    pub fn family(&self) -> &str {
        &self.family
    }
    /// Exact requested model.
    #[must_use]
    pub fn requested_model(&self) -> &str {
        &self.requested_model
    }
    /// Exact effective model.
    #[must_use]
    pub fn effective_model(&self) -> &str {
        &self.effective_model
    }
    /// Exact effective provider-configuration identity.
    #[must_use]
    pub const fn effective_provider_configuration(&self) -> EffectiveProviderConfigurationId {
        self.effective_provider_configuration
    }
    /// Closed catalog revision.
    #[must_use]
    pub const fn projection_revision(&self) -> u32 {
        self.projection_revision
    }
}

/// One V2 compiled view from an exact exposed name to one canonical capability/executor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolProjectionExposureV2 {
    canonical_operations: BTreeMap<CanonicalProcessOperationV2, RegisteredToolIdentity>,
    source_canonical_name: ToolName,
    executor_id: crate::ToolExecutorId,
    exposed_name: ToolName,
    description: String,
    presentation: ProjectedToolPresentationV2,
    defaults: SemanticJsonV1,
    lowering: ToolProjectionLoweringV2,
    lowering_identity: Digest,
    output_schema: PortableToolSchemaV2,
    raising: ToolProjectionRaisingV2,
    raising_identity: Digest,
    target: ProviderModelProjectionIdentityV2,
    identity: Digest,
}

impl ToolProjectionExposureV2 {
    /// Constructs and identity-binds one closed projection mapping.
    ///
    /// # Errors
    ///
    /// Rejects invalid names, schemas, defaults, descriptions, or digest inputs.
    #[allow(
        clippy::too_many_arguments,
        reason = "every behavior-bearing projection axis is explicit"
    )]
    pub(crate) fn new(
        canonical_operations: impl IntoIterator<
            Item = (CanonicalProcessOperationV2, RegisteredToolIdentity),
        >,
        exposed_name: impl Into<String>,
        description: impl Into<String>,
        input_schema: PortableToolSchemaV2,
        defaults: SemanticJsonV1,
        lowering: ToolProjectionLoweringV2,
        output_schema: PortableToolSchemaV2,
        raising: ToolProjectionRaisingV2,
        target: ProviderModelProjectionIdentityV2,
    ) -> Result<Self, ToolRuntimeError> {
        Self::new_with_presentation(
            canonical_operations,
            exposed_name,
            description,
            ProjectedToolPresentationV2::StructuredFunction { input_schema },
            defaults,
            lowering,
            output_schema,
            raising,
            target,
        )
    }

    /// Constructs an explicit bounded UTF-8 custom-tool projection.
    ///
    /// This is a presentation/descriptor primitive only. It does not enroll a
    /// composition runtime or a provider/model candidate.
    #[allow(
        clippy::too_many_arguments,
        reason = "every behavior-bearing projection axis is explicit"
    )]
    pub(crate) fn new_custom(
        canonical_operations: impl IntoIterator<
            Item = (CanonicalProcessOperationV2, RegisteredToolIdentity),
        >,
        exposed_name: impl Into<String>,
        description: impl Into<String>,
        input: Utf8TextContractV1,
        format: ProjectedCustomToolFormatV2,
        defaults: SemanticJsonV1,
        lowering: ToolProjectionLoweringV2,
        output_schema: PortableToolSchemaV2,
        raising: ToolProjectionRaisingV2,
        target: ProviderModelProjectionIdentityV2,
    ) -> Result<Self, ToolRuntimeError> {
        Self::new_with_presentation(
            canonical_operations,
            exposed_name,
            description,
            ProjectedToolPresentationV2::CustomUtf8 { input, format },
            defaults,
            lowering,
            output_schema,
            raising,
            target,
        )
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "every behavior-bearing projection axis is explicit"
    )]
    #[allow(
        clippy::too_many_lines,
        reason = "projection identity binds its closed contracts atomically"
    )]
    fn new_with_presentation(
        canonical_operations: impl IntoIterator<
            Item = (CanonicalProcessOperationV2, RegisteredToolIdentity),
        >,
        exposed_name: impl Into<String>,
        description: impl Into<String>,
        presentation: ProjectedToolPresentationV2,
        defaults: SemanticJsonV1,
        lowering: ToolProjectionLoweringV2,
        output_schema: PortableToolSchemaV2,
        raising: ToolProjectionRaisingV2,
        target: ProviderModelProjectionIdentityV2,
    ) -> Result<Self, ToolRuntimeError> {
        let canonical_operations = canonical_operations.into_iter().collect::<BTreeMap<_, _>>();
        if canonical_operations.is_empty() {
            return Err(registration_error(
                "projection must bind at least one canonical process operation",
            ));
        }
        let source = canonical_operations
            .values()
            .next()
            .ok_or_else(|| registration_error("projection operation set is empty"))?;
        let source_canonical_name = source.name().clone();
        let executor_id = source.executor_id().clone();
        let exposed_name = ToolName::new(exposed_name.into()).map_err(|error| {
            ToolRuntimeError::from_sensitive_source(
                ToolRuntimeErrorKind::Registration,
                "projected tool name is invalid",
                &error,
            )
        })?;
        let description = description.into();
        let defaults_object = defaults
            .value()
            .as_object()
            .ok_or_else(|| registration_error("projection defaults must be a JSON object"))?;
        match &presentation {
            ProjectedToolPresentationV2::StructuredFunction { input_schema } => {
                validate_projection_defaults(input_schema, defaults_object)?;
            }
            ProjectedToolPresentationV2::CustomUtf8 { .. } if !defaults_object.is_empty() => {
                return Err(registration_error(
                    "custom-tool projection defaults must be empty",
                ));
            }
            ProjectedToolPresentationV2::CustomUtf8 { .. } => {}
        }
        if description.is_empty()
            || description.len() > ProjectedToolDefinitionV2::MAX_DESCRIPTION_BYTES
        {
            return Err(registration_error(
                "projected tool description is empty or exceeds its global bound",
            ));
        }
        if raising == ToolProjectionRaisingV2::TextResultObjectV1
            && !admits_every_text_result(&output_schema)
        {
            return Err(registration_error(
                "text-result raising requires a closed object schema with one string result",
            ));
        }
        // The raw UTF-8 wire shape is executable behavior. Rotate only these
        // lowerings so a persisted descriptor from the JSON-wrapping version
        // cannot reopen against the corrected implementation.
        let lowering_program = match (&presentation, lowering) {
            (_, ToolProjectionLoweringV2::CodexApplyPatch) => {
                "codex-envelope-to-raw-utf8-apply-patch-v2"
            }
            (
                ProjectedToolPresentationV2::CustomUtf8 { .. },
                ToolProjectionLoweringV2::CanonicalPassthrough,
            ) => "custom-raw-utf8-canonical-passthrough-v2",
            _ => lowering.operation(),
        };
        let lowering_identity = projection_program_identity(
            b"lowering",
            lowering_program.as_bytes(),
            defaults.digest(),
        )?;
        // Claude's numbered read result is a distinct executable raising program.
        // Bind its semantics without changing nested raw JSON or unrelated tools.
        let raising_program = match (lowering, raising) {
            (ToolProjectionLoweringV2::ClaudeBash, ToolProjectionRaisingV2::ProcessResult) => {
                "claude_native_promotion_text_v2"
            }
            (
                ToolProjectionLoweringV2::GrokRunTerminalCommand,
                ToolProjectionRaisingV2::ProcessResult,
            ) => "grok_running_task_id_v2",
            (
                ToolProjectionLoweringV2::ClaudeReadFile,
                ToolProjectionRaisingV2::TextResultObjectV1,
            ) => "claude_cat_n_lossless_text_result_object_v2",
            (ToolProjectionLoweringV2::ClaudeReadFile, ToolProjectionRaisingV2::Identity) => {
                "claude_cat_n_lossless_identity_v2"
            }
            (ToolProjectionLoweringV2::PiRead, ToolProjectionRaisingV2::TextResultObjectV1) => {
                "pi_read_text_result_object_v1"
            }
            (ToolProjectionLoweringV2::PiRead, ToolProjectionRaisingV2::Identity) => {
                "pi_read_identity_v1"
            }
            (ToolProjectionLoweringV2::PiBash, ToolProjectionRaisingV2::ProcessResult) => {
                "pi_background_notice_text_v1"
            }
            _ => raising.operation(),
        };
        let raising_identity = projection_program_identity(
            b"raising",
            raising_program.as_bytes(),
            output_schema.digest(),
        )?;
        let identity = projection_entry_identity(
            &canonical_operations,
            &exposed_name,
            &description,
            &presentation,
            lowering_identity,
            &output_schema,
            raising_identity,
            &target,
        )?;
        Ok(Self {
            canonical_operations,
            source_canonical_name,
            executor_id,
            exposed_name,
            description,
            presentation,
            defaults,
            lowering,
            lowering_identity,
            output_schema,
            raising,
            raising_identity,
            target,
            identity,
        })
    }

    /// Exact canonical capability identity.
    #[must_use]
    pub fn canonical_operations(
        &self,
    ) -> &BTreeMap<CanonicalProcessOperationV2, RegisteredToolIdentity> {
        &self.canonical_operations
    }
    /// Exact projected name.
    #[must_use]
    pub const fn exposed_name(&self) -> &ToolName {
        &self.exposed_name
    }
    /// Model-visible description.
    #[must_use]
    pub fn description(&self) -> &str {
        &self.description
    }
    /// Exact provider-visible presentation.
    #[must_use]
    pub const fn presentation(&self) -> &ProjectedToolPresentationV2 {
        &self.presentation
    }
    /// V2 projected input schema, only for a structured-function presentation.
    #[must_use]
    pub const fn input_schema(&self) -> Option<&PortableToolSchemaV2> {
        self.presentation.input_schema()
    }
    /// Explicit defaults applied by lowering.
    #[must_use]
    pub const fn defaults(&self) -> &SemanticJsonV1 {
        &self.defaults
    }
    /// Closed lowering program identity.
    #[must_use]
    pub const fn lowering_identity(&self) -> Digest {
        self.lowering_identity
    }
    /// Returns the lowering rule this entry applies to model arguments.
    #[must_use]
    pub const fn lowering(&self) -> ToolProjectionLoweringV2 {
        self.lowering
    }
    /// V2 raised output schema.
    #[must_use]
    pub const fn output_schema(&self) -> &PortableToolSchemaV2 {
        &self.output_schema
    }
    /// Closed raising program identity.
    #[must_use]
    pub const fn raising_identity(&self) -> Digest {
        self.raising_identity
    }
    /// Returns the raising rule this entry applies to canonical outcomes.
    #[must_use]
    pub const fn raising(&self) -> ToolProjectionRaisingV2 {
        self.raising
    }
    /// Exact provider/model target.
    #[must_use]
    pub const fn target(&self) -> &ProviderModelProjectionIdentityV2 {
        &self.target
    }
    /// Exact operation effect classification.
    #[must_use]
    pub fn canonical_identity(
        &self,
        operation: CanonicalProcessOperationV2,
    ) -> &RegisteredToolIdentity {
        &self.canonical_operations[&operation]
    }
    /// Shared canonical executor binding for every allowed operation.
    #[must_use]
    pub fn executor_id(&self) -> &crate::ToolExecutorId {
        &self.executor_id
    }
    /// Complete projection mapping identity.
    #[must_use]
    pub const fn identity(&self) -> Digest {
        self.identity
    }
}

/// Receipt proving which exact compiled view produced a canonical invocation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolProjectionReceiptV2 {
    projection_identity: Digest,
    exposure_map_identity: Digest,
    canonical_identity: RegisteredToolIdentity,
    canonical_operation: CanonicalProcessOperationV2,
    exposed_name: ToolName,
    lowering_identity: Digest,
    raising_identity: Digest,
    target: ProviderModelProjectionIdentityV2,
}

impl ToolProjectionReceiptV2 {
    /// Exact projection entry identity.
    #[must_use]
    pub const fn projection_identity(&self) -> Digest {
        self.projection_identity
    }
    /// Exact complete projection-map identity.
    #[must_use]
    pub const fn exposure_map_identity(&self) -> Digest {
        self.exposure_map_identity
    }
    /// Exact canonical capability selected.
    #[must_use]
    pub const fn canonical_identity(&self) -> &RegisteredToolIdentity {
        &self.canonical_identity
    }
    /// Exact canonical operation selected by validated lowering.
    #[must_use]
    pub const fn canonical_operation(&self) -> CanonicalProcessOperationV2 {
        self.canonical_operation
    }
    /// Provider-visible name received.
    #[must_use]
    pub const fn exposed_name(&self) -> &ToolName {
        &self.exposed_name
    }
    /// Deterministic lowering identity.
    #[must_use]
    pub const fn lowering_identity(&self) -> Digest {
        self.lowering_identity
    }
    /// Deterministic raising identity.
    #[must_use]
    pub const fn raising_identity(&self) -> Digest {
        self.raising_identity
    }
    /// Exact qualified tuple.
    #[must_use]
    pub const fn target(&self) -> &ProviderModelProjectionIdentityV2 {
        &self.target
    }
}

/// Canonical invocation plus an exact receipt when its exposed name was projected.
pub struct ProjectedToolInvocationV2 {
    invocation: ToolInvocation,
    receipt: Option<ToolProjectionReceiptV2>,
}

impl ProjectedToolInvocationV2 {
    /// Borrows the canonical invocation.
    #[must_use]
    pub const fn invocation(&self) -> &ToolInvocation {
        &self.invocation
    }
    /// Borrows the exact projection receipt, absent for an additive canonical tool.
    #[must_use]
    pub const fn receipt(&self) -> Option<&ToolProjectionReceiptV2> {
        self.receipt.as_ref()
    }
    /// Splits the value for the agent runtime's canonical dispatch path.
    #[must_use]
    pub fn into_parts(self) -> (ToolInvocation, Option<ToolProjectionReceiptV2>) {
        (self.invocation, self.receipt)
    }
}

/// One requested provider-facing name for an exact registered tool identity.
///
/// Construction validates the portable exposed name. Registry ownership,
/// reserved namespaces, and collisions are validated when the exposure map is
/// frozen.
#[derive(Clone, Eq, PartialEq)]
pub struct ToolExposure {
    canonical_identity: RegisteredToolIdentity,
    exposed_name: ToolName,
    description_override: Option<String>,
}

impl ToolExposure {
    /// Creates a requested exposure for one frozen registered identity.
    ///
    /// # Errors
    ///
    /// Returns a registration error when the exposed name is not a portable
    /// [`ToolName`].
    pub fn new(
        canonical_identity: RegisteredToolIdentity,
        exposed_name: impl Into<String>,
    ) -> Result<Self, ToolRuntimeError> {
        let exposed_name = ToolName::new(exposed_name.into()).map_err(|error| {
            ToolRuntimeError::from_sensitive_source(
                ToolRuntimeErrorKind::Registration,
                "exposed tool name is invalid",
                &error,
            )
        })?;
        Ok(Self {
            canonical_identity,
            exposed_name,
            description_override: None,
        })
    }

    /// Applies an optional model-facing description without changing executable identity.
    ///
    /// # Errors
    ///
    /// Returns a registration error for an empty override. Full definition
    /// bounds are revalidated when the exposure map is frozen.
    pub fn with_description_override(
        mut self,
        description: Option<String>,
    ) -> Result<Self, ToolRuntimeError> {
        if description.as_deref().is_some_and(str::is_empty) {
            return Err(registration_error(
                "exposed tool description override is empty",
            ));
        }
        self.description_override = description;
        Ok(self)
    }

    /// Returns the exact canonical identity selected from a frozen registry.
    #[must_use]
    pub const fn canonical_identity(&self) -> &RegisteredToolIdentity {
        &self.canonical_identity
    }

    /// Returns the provider-facing name.
    #[must_use]
    pub const fn exposed_name(&self) -> &ToolName {
        &self.exposed_name
    }

    /// Returns the optional provider-facing description override.
    #[must_use]
    pub fn description_override(&self) -> Option<&str> {
        self.description_override.as_deref()
    }
}

impl fmt::Debug for ToolExposure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ToolExposure")
            .field("canonical_identity", &self.canonical_identity)
            .field("exposed_name", &self.exposed_name)
            .finish_non_exhaustive()
    }
}

struct FrozenExposureMap {
    registry: FrozenToolRegistry,
    entries: Arc<[ToolExposure]>,
    definitions: Arc<[ToolDefinition]>,
    by_canonical: BTreeMap<ToolName, usize>,
    by_exposed: BTreeMap<ToolName, usize>,
    registry_digest: Digest,
    digest: Digest,
}

/// Immutable per-run mapping between registered and provider-facing tool names.
///
/// The map may select a subset of a registry. Every selected entry remains
/// bound to its exact registry allocation and complete frozen identity. Its
/// deterministic digest binds the exact exposed set; unrelated unexposed
/// registry entries do not invalidate request prefixes or effect decisions.
#[derive(Clone)]
pub struct ToolExposureMap {
    inner: Arc<FrozenExposureMap>,
}

impl ToolExposureMap {
    /// Validates requested exposures and freezes their bidirectional lookup.
    ///
    /// Input order does not affect entry order or the resulting digest.
    /// Omitting a registry entry is allowed; profiles use omission to select a
    /// provider-facing subset.
    ///
    /// # Errors
    ///
    /// Returns a registration error for a missing or foreign canonical
    /// identity, duplicate canonical entry, exposed alias collision, or name in
    /// a namespace reserved by the source registry.
    pub fn new(
        registry: FrozenToolRegistry,
        exposures: impl IntoIterator<Item = ToolExposure>,
    ) -> Result<Self, ToolRuntimeError> {
        let mut entries = exposures.into_iter().collect::<Vec<_>>();
        entries.sort_by(|left, right| {
            left.canonical_identity
                .name()
                .cmp(right.canonical_identity.name())
        });

        let mut by_canonical = BTreeMap::new();
        let mut by_exposed = BTreeMap::new();
        let mut definitions = Vec::with_capacity(entries.len());
        for (index, exposure) in entries.iter().enumerate() {
            if !registry.owns_identity(&exposure.canonical_identity) {
                return Err(registration_error(
                    "exposed tool identity is missing from or foreign to the registry",
                ));
            }
            if registry.exposed_name_is_reserved(&exposure.exposed_name) {
                return Err(registration_error(
                    "exposed tool name uses a reserved namespace",
                ));
            }
            if by_canonical
                .insert(exposure.canonical_identity.name().clone(), index)
                .is_some()
            {
                return Err(registration_error("duplicate canonical tool exposure"));
            }
            if by_exposed
                .insert(exposure.exposed_name.clone(), index)
                .is_some()
            {
                return Err(registration_error("exposed tool alias collision"));
            }

            let canonical = registry
                .definition(exposure.canonical_identity.name())
                .ok_or_else(|| registration_error("exposed tool definition is missing"))?;
            definitions.push(exposed_definition(canonical, exposure)?);
        }

        let registry_digest = registry_identity_digest(&registry)?;
        let digest = exposure_map_digest(&entries, &definitions)?;
        Ok(Self {
            inner: Arc::new(FrozenExposureMap {
                registry,
                entries: entries.into(),
                definitions: definitions.into(),
                by_canonical,
                by_exposed,
                registry_digest,
                digest,
            }),
        })
    }

    /// Returns selected exposures in deterministic canonical-name order.
    #[must_use]
    pub fn entries(&self) -> &[ToolExposure] {
        &self.inner.entries
    }

    /// Returns provider-facing definitions aligned with [`Self::entries`].
    ///
    /// Name and an explicit description override may differ from the source;
    /// schema, semantic version, concurrency, and effect metadata are preserved.
    #[must_use]
    pub fn definitions(&self) -> &[ToolDefinition] {
        &self.inner.definitions
    }

    /// Returns the number of selected exposures.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.entries.len()
    }

    /// Reports whether the provider-facing selection is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inner.entries.is_empty()
    }

    /// Looks up an exposure by its exact registered identity.
    #[must_use]
    pub fn by_canonical(&self, identity: &RegisteredToolIdentity) -> Option<&ToolExposure> {
        if !self.inner.registry.owns_identity(identity) {
            return None;
        }
        self.inner
            .by_canonical
            .get(identity.name())
            .and_then(|index| self.inner.entries.get(*index))
            .filter(|entry| entry.canonical_identity == *identity)
    }

    /// Looks up an exposure by provider-facing name.
    #[must_use]
    pub fn by_exposed(&self, exposed_name: &ToolName) -> Option<&ToolExposure> {
        self.inner
            .by_exposed
            .get(exposed_name)
            .and_then(|index| self.inner.entries.get(*index))
    }

    /// Resolves an exact registered identity to its provider-facing name.
    #[must_use]
    pub fn exposed_name(&self, identity: &RegisteredToolIdentity) -> Option<&ToolName> {
        self.by_canonical(identity).map(ToolExposure::exposed_name)
    }

    /// Resolves a provider-facing name to its exact registered identity.
    #[must_use]
    pub fn canonical_identity(&self, exposed_name: &ToolName) -> Option<&RegisteredToolIdentity> {
        self.by_exposed(exposed_name)
            .map(ToolExposure::canonical_identity)
    }

    /// Returns the deterministic identity of the complete source registry.
    #[must_use]
    pub fn registry_digest(&self) -> Digest {
        self.inner.registry_digest
    }

    /// Reports whether this map was frozen from the exact executable registry
    /// supplied by the caller.
    ///
    /// This is stronger than digest equality and prevents a map from preparing
    /// invocations whose private provenance cannot be executed by another
    /// otherwise identical registry allocation.
    #[must_use]
    pub fn is_for_registry(&self, registry: &FrozenToolRegistry) -> bool {
        self.inner.registry.is_same_snapshot(registry)
    }

    /// Returns the request-prefix binding for this exact exposed tool set.
    ///
    /// The digest changes when a selected canonical identity/version, schema,
    /// executor, exposed definition, or exposed name changes. Unselected
    /// registry entries are intentionally excluded.
    #[must_use]
    pub fn digest(&self) -> Digest {
        self.inner.digest
    }

    pub(crate) fn registry(&self) -> &FrozenToolRegistry {
        &self.inner.registry
    }

    /// Resolves an exposed request and validates it against the canonical schema.
    ///
    /// The returned invocation retains the exact registered canonical identity;
    /// the provider-facing alias never becomes executable identity.
    ///
    /// # Errors
    ///
    /// Returns an unknown-tool error for an unexposed name, or the canonical
    /// registry's validation error for malformed or schema-invalid arguments.
    pub fn prepare(
        &self,
        request: ToolInvocationRequest,
    ) -> Result<ToolInvocation, ToolRuntimeError> {
        let exposed_name = request.tool().clone();
        let identity = self.canonical_identity(request.tool()).ok_or_else(|| {
            ToolRuntimeError::new(
                ToolRuntimeErrorKind::UnknownTool,
                "tool name is not exposed by this map",
            )
        })?;
        let invocation = self
            .inner
            .registry
            .prepare(request.with_tool(identity.name().clone()))?;
        Ok(invocation.with_exposure(exposed_name, self.digest()))
    }
}

impl fmt::Debug for ToolExposureMap {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ToolExposureMap")
            .field("entry_count", &self.len())
            .field("registry_digest", &self.registry_digest())
            .field("digest", &self.digest())
            .finish()
    }
}

/// Immutable V2 catalog allowing several exact projected names for one canonical capability.
#[derive(Clone)]
pub struct ToolProjectionMapV2 {
    registry: FrozenToolRegistry,
    target: ProviderModelProjectionIdentityV2,
    entries: Arc<[ToolProjectionExposureV2]>,
    by_exposed: BTreeMap<ToolName, usize>,
    covered_canonical_names: BTreeSet<ToolName>,
    registry_digest: Digest,
    digest: Digest,
}

impl ToolProjectionMapV2 {
    /// Freezes one exact-target projection set.
    ///
    /// Multiple entries may bind the same canonical capability. Exposed-name
    /// collisions, foreign identities, target drift, executor drift, and
    /// canonical operation disagreement fail before provider dispatch.
    ///
    /// # Errors
    ///
    /// Rejects every collision, foreign binding, target drift, or operation mismatch.
    pub(crate) fn new(
        registry: FrozenToolRegistry,
        target: ProviderModelProjectionIdentityV2,
        entries: impl IntoIterator<Item = ToolProjectionExposureV2>,
    ) -> Result<Self, ToolRuntimeError> {
        let entries = entries.into_iter().collect::<Vec<_>>();
        let mut by_exposed = BTreeMap::new();
        let mut covered_canonical_names = BTreeSet::new();
        for (index, entry) in entries.iter().enumerate() {
            if entry.target != target || registry.exposed_name_is_reserved(&entry.exposed_name) {
                return Err(registration_error(
                    "projection entry is foreign to its registry or exact target",
                ));
            }
            if matches!(
                entry.presentation(),
                ProjectedToolPresentationV2::CustomUtf8 { .. }
            ) && !matches!(
                entry.lowering,
                ToolProjectionLoweringV2::CodexApplyPatch
                    | ToolProjectionLoweringV2::CanonicalPassthrough
            ) {
                return Err(registration_error(
                    "custom projection has no UTF-8 lowering",
                ));
            }
            let mut executor = None;
            for (operation, identity) in &entry.canonical_operations {
                if !registry.owns_identity(identity) {
                    return Err(registration_error(
                        "projection operation identity is foreign to its registry",
                    ));
                }
                let definition = registry.definition(identity.name()).ok_or_else(|| {
                    registration_error("canonical projection operation is absent")
                })?;
                if entry.lowering == ToolProjectionLoweringV2::CodexApplyPatch
                    && !matches!(definition.input(), ToolInputContract::Utf8(_))
                {
                    return Err(registration_error(
                        "Codex patch projection requires canonical UTF-8 input",
                    ));
                }
                if let ProjectedToolPresentationV2::CustomUtf8 { input, .. } = &entry.presentation
                    && !matches!(definition.input(), ToolInputContract::Utf8(bound) if bound == input)
                {
                    return Err(registration_error(
                        "custom projection does not match its canonical UTF-8 contract",
                    ));
                }
                let expected = if *operation == CanonicalProcessOperationV2::Poll {
                    ToolEffect::ReadOnly
                } else {
                    ToolEffect::Mutating
                };
                if definition.effect() != expected {
                    return Err(registration_error(
                        "canonical process operation has an unsafe effect class",
                    ));
                }
                if executor
                    .replace(identity.executor_id())
                    .is_some_and(|prior| prior != identity.executor_id())
                {
                    return Err(registration_error(
                        "canonical process operations do not share one executor",
                    ));
                }
                covered_canonical_names.insert(identity.name().clone());
            }
            if by_exposed
                .insert(entry.exposed_name.clone(), index)
                .is_some()
            {
                return Err(registration_error("projected tool name collision"));
            }
        }
        let registry_digest = registry_identity_digest(&registry)?;
        let digest = projection_map_digest(registry_digest, &target, &entries)?;
        Ok(Self {
            registry,
            target,
            entries: entries.into(),
            by_exposed,
            covered_canonical_names,
            registry_digest,
            digest,
        })
    }

    /// Exact qualified provider/model tuple.
    #[must_use]
    pub const fn target(&self) -> &ProviderModelProjectionIdentityV2 {
        &self.target
    }
    /// Entries in caller presentation order. Identity remains set-ordered.
    #[must_use]
    pub fn entries(&self) -> &[ToolProjectionExposureV2] {
        &self.entries
    }
    /// Looks up one exact exposed projection name.
    #[must_use]
    pub fn by_exposed(&self, name: &ToolName) -> Option<&ToolProjectionExposureV2> {
        self.by_exposed
            .get(name)
            .and_then(|index| self.entries.get(*index))
    }
    /// Complete canonical registry identity.
    #[must_use]
    pub const fn registry_digest(&self) -> Digest {
        self.registry_digest
    }
    /// Complete exact candidate projection-map identity.
    #[must_use]
    pub const fn digest(&self) -> Digest {
        self.digest
    }
    /// Reports exact executable snapshot ownership.
    #[must_use]
    pub fn is_for_registry(&self, registry: &FrozenToolRegistry) -> bool {
        self.registry.is_same_snapshot(registry)
    }

    fn unknown_presented_tool(&self, name: &ToolName) -> ToolRuntimeError {
        let presented = self
            .entries
            .iter()
            .map(|entry| entry.exposed_name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let message = if self.covered_canonical_names.contains(name) {
            format!("canonical tool is not exposed; presented tools: {presented}")
        } else {
            format!("tool is not in the presented catalog; presented tools: {presented}")
        };
        ToolRuntimeError::new(ToolRuntimeErrorKind::UnknownTool, message)
    }

    /// Converts this exact tools-owned map into the model request's compiled view.
    ///
    /// Every target and definition axis is copied directly from the frozen map;
    /// this boundary performs no provider inference, qualification, or fallback.
    ///
    /// # Errors
    ///
    /// Returns a registration error if the model-owned request view rejects a
    /// tools-owned invariant.
    pub fn compile_request_view_v2(&self) -> Result<CompiledToolProjectionV2, ToolRuntimeError> {
        let target = ToolProjectionTargetV2::new(
            self.target.integration_class,
            self.target.protocol.clone(),
            self.target.route.clone(),
            self.target.integration_profile.clone(),
            self.target.family.clone(),
            self.target.requested_model.clone(),
            self.target.effective_model.clone(),
            self.target.effective_provider_configuration,
            self.target.projection_revision,
        )
        .map_err(|error| compiled_projection_error(&error))?;
        let definitions = self
            .entries
            .iter()
            .map(|entry| {
                let canonical_name = entry.source_canonical_name.clone();
                let covered = entry
                    .canonical_operations
                    .values()
                    .map(|identity| identity.name().clone())
                    .collect();
                match &entry.presentation {
                    ProjectedToolPresentationV2::StructuredFunction { input_schema } => {
                        ProjectedToolDefinitionV2::new(
                            canonical_name,
                            covered,
                            entry.exposed_name.clone(),
                            entry.description.clone(),
                            input_schema.clone(),
                            entry.output_schema.clone(),
                            entry.identity,
                        )
                    }
                    ProjectedToolPresentationV2::CustomUtf8 { input, format } => {
                        ProjectedToolDefinitionV2::new_custom(
                            canonical_name,
                            covered,
                            entry.exposed_name.clone(),
                            entry.description.clone(),
                            *input,
                            format.clone(),
                            entry.output_schema.clone(),
                            entry.identity,
                        )
                    }
                }
                .map_err(|error| compiled_projection_error(&error))
            })
            .collect::<Result<Vec<_>, _>>()?;
        CompiledToolProjectionV2::new(target, self.registry_digest, self.digest, definitions)
            .map_err(|error| compiled_projection_error(&error))
    }

    /// Validates and lowers one projected request to a canonical invocation plus receipt.
    ///
    /// # Errors
    ///
    /// Rejects unknown names, invalid V2 input, or canonical validation failure.
    pub fn prepare(
        &self,
        request: ToolInvocationRequest,
    ) -> Result<ProjectedToolInvocationV2, ToolRuntimeError> {
        self.prepare_inner(request, None)
    }

    /// Validates and lowers one projected request against its exact live workspace.
    ///
    /// Provider-native process interfaces may conventionally emit an absolute
    /// working directory. Plakous accepts that presentation only when it names
    /// the active cwd or one of its descendants, then persists and dispatches
    /// the workspace-relative canonical form.
    ///
    /// # Errors
    ///
    /// Rejects unknown names, invalid V2 input, or a working directory outside
    /// the exact execution environment.
    pub fn prepare_in_environment(
        &self,
        request: ToolInvocationRequest,
        environment: &ExecutionEnvironment,
    ) -> Result<ProjectedToolInvocationV2, ToolRuntimeError> {
        self.prepare_inner(request, Some(environment))
    }

    #[allow(
        clippy::needless_pass_by_value,
        reason = "prepare consumes the caller-owned request even when only borrowed for lowering"
    )]
    fn prepare_inner(
        &self,
        request: ToolInvocationRequest,
        environment: Option<&ExecutionEnvironment>,
    ) -> Result<ProjectedToolInvocationV2, ToolRuntimeError> {
        let Some(entry) = self.by_exposed(request.tool()) else {
            return Err(self.unknown_presented_tool(request.tool()));
        };
        let projected = match entry.presentation() {
            ProjectedToolPresentationV2::StructuredFunction { .. } => {
                let input_schema = entry.input_schema().ok_or_else(|| {
                    registration_error("structured projection is missing its input schema")
                })?;
                // An accepted superset, applied before validation and only for
                // the composition `wait` markers, recognised by canonical
                // identity: a `cell_id` the model echoed as the numeral it was
                // shown becomes the string the contract declares. The presented
                // schema is unchanged and still decides. No active cell is known
                // here; the agent driver applies the same normalisation with the
                // running cell in hand before it builds the request, and this
                // call is idempotent over that.
                let raw = entry
                    .canonical_operations()
                    .values()
                    .all(crate::composition_markers::is_cell_wait_marker)
                    .then(|| {
                        crate::composition_markers::normalized_wait_arguments(
                            request.raw_arguments(),
                            None,
                        )
                    })
                    .flatten();
                input_schema
                    .parse_instance(raw.as_deref().unwrap_or_else(|| request.raw_arguments()))
                    .map_err(|error| {
                        if let Some(fields) = missing_required_fields(
                            input_schema,
                            raw.as_deref().unwrap_or_else(|| request.raw_arguments()),
                        ) {
                            return ToolRuntimeError::new(
                                ToolRuntimeErrorKind::InvalidArguments,
                                format!(
                                    "projected tool input is missing required fields: {fields}"
                                ),
                            );
                        }
                        ToolRuntimeError::from_sensitive_source(
                            ToolRuntimeErrorKind::InvalidArguments,
                            "projected tool input violates its V2 contract",
                            &error,
                        )
                    })?
                    .value()
                    .clone()
            }
            ProjectedToolPresentationV2::CustomUtf8 { input, .. } => {
                input.validate(request.raw_arguments()).map_err(|error| {
                    ToolRuntimeError::from_sensitive_source(
                        ToolRuntimeErrorKind::InvalidArguments,
                        "projected custom tool input violates its UTF-8 contract",
                        &error,
                    )
                })?;
                Arc::new(Value::String(request.raw_arguments().to_owned()))
            }
        };
        let (operation, canonical) = lower_projection(entry, projected.as_ref(), environment)?;
        let canonical_identity = entry.canonical_identity(operation);
        let exposed_name = request.tool().clone();
        let canonical_arguments = canonical.into_request_arguments()?;
        let canonical_request = ToolInvocationRequest::new(
            request.run_id(),
            request.turn_id(),
            request.attempt_id(),
            request.call_id(),
            canonical_identity.name().clone(),
            request.correlation().clone(),
            canonical_arguments,
        );
        let invocation = self
            .registry
            .prepare(canonical_request)?
            .with_exposure(exposed_name.clone(), self.digest);
        Ok(ProjectedToolInvocationV2 {
            invocation,
            receipt: Some(ToolProjectionReceiptV2 {
                projection_identity: entry.identity,
                exposure_map_identity: self.digest,
                canonical_identity: canonical_identity.clone(),
                canonical_operation: operation,
                exposed_name,
                lowering_identity: entry.lowering_identity,
                raising_identity: entry.raising_identity,
                target: entry.target.clone(),
            }),
        })
    }

    /// Deterministically validates and raises one canonical result for its exact receipt.
    ///
    /// # Errors
    ///
    /// Rejects foreign receipts and results that violate the raised schema.
    pub fn raise(
        &self,
        receipt: &ToolProjectionReceiptV2,
        outcome: ToolOutcome,
    ) -> Result<ToolOutcome, ToolRuntimeError> {
        let presentation_subject = outcome.presentation_subject().map(ToOwned::to_owned);
        let mut raised = self.raise_preserving_subject(receipt, outcome)?;
        if raised.presentation_subject().is_none()
            && let Some(subject) = presentation_subject
        {
            raised = raised.with_presentation_subject(subject);
        }
        Ok(raised)
    }

    fn raise_preserving_subject(
        &self,
        receipt: &ToolProjectionReceiptV2,
        outcome: ToolOutcome,
    ) -> Result<ToolOutcome, ToolRuntimeError> {
        if receipt.exposure_map_identity != self.digest || receipt.target != self.target {
            return Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::Execution,
                "projection receipt is foreign to this exact exposure map",
            ));
        }
        let entry = self.by_exposed(&receipt.exposed_name).ok_or_else(|| {
            ToolRuntimeError::new(
                ToolRuntimeErrorKind::Execution,
                "projection receipt is stale",
            )
        })?;
        if entry.identity != receipt.projection_identity
            || entry.canonical_identity(receipt.canonical_operation) != &receipt.canonical_identity
            || entry.lowering_identity != receipt.lowering_identity
            || entry.raising_identity != receipt.raising_identity
        {
            return Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::Execution,
                "projection receipt identity mismatch",
            ));
        }
        if matches!(
            entry.lowering,
            ToolProjectionLoweringV2::ClaudeReadFile | ToolProjectionLoweringV2::PiRead
        ) && entry.raising != ToolProjectionRaisingV2::RawUtf8V1
            && outcome.kind() == crate::ToolOutcomeKind::Success
        {
            return raise_read_observation(entry, &outcome);
        }
        if entry.raising == ToolProjectionRaisingV2::Identity {
            return raise_identity(entry, outcome);
        }
        if entry.raising == ToolProjectionRaisingV2::ModelContentIdentityV1 {
            return validate_model_content_identity(
                &self.registry,
                &receipt.canonical_identity,
                outcome,
            );
        }
        if entry.raising == ToolProjectionRaisingV2::TextResultObjectV1 {
            return raise_text_result_object(outcome);
        }
        if entry.raising == ToolProjectionRaisingV2::RawUtf8V1 {
            return Ok(outcome);
        }
        if outcome.kind() == crate::ToolOutcomeKind::Success {
            return raise_process_result(entry, &outcome);
        }
        if outcome.kind() == crate::ToolOutcomeKind::Failure
            && entry.lowering == ToolProjectionLoweringV2::CodexWriteStdin
            && matches!(outcome.parts(), [ToolResultPart::Text(text)]
                if text.text() == crate::environment::JOINED_STDIN_CLOSED)
        {
            let mut refusal = ToolOutcome::failure_text(
                "stdin is closed for this session; rerun exec_command with tty=true to keep stdin open",
            )?;
            if let Some(update) = outcome.process_session_update() {
                refusal = refusal.with_process_session_update(update);
            }
            refusal.process_telemetry = outcome.process_telemetry;
            return Ok(refusal);
        }
        Ok(outcome)
    }

    pub(crate) const fn registry(&self) -> &FrozenToolRegistry {
        &self.registry
    }
}

/// Raises one canonical process result into the projection's shared shape.
#[allow(
    clippy::too_many_lines,
    reason = "each vendor's background and status lines stay beside the shared shape"
)]
fn raise_process_result(
    entry: &ToolProjectionExposureV2,
    outcome: &ToolOutcome,
) -> Result<ToolOutcome, ToolRuntimeError> {
    let [ToolResultPart::Text(text)] = outcome.parts() else {
        return Err(ToolRuntimeError::new(
            ToolRuntimeErrorKind::Execution,
            "canonical projection result is not one structured text value",
        ));
    };
    let canonical: CanonicalProcessResultWireV2 =
        serde_json::from_str(text.text()).map_err(|error| {
            ToolRuntimeError::from_sensitive_source(
                ToolRuntimeErrorKind::Execution,
                "canonical process result is malformed",
                &error,
            )
        })?;
    canonical.validate()?;
    let status = raised_process_status(&canonical.status);
    let mut output = canonical.output;
    if entry.lowering == ToolProjectionLoweringV2::PiBash
        && let Some(id) = canonical.session_id
        && let Some(hint) = canonical.foreground_hint_ms
    {
        if !output.is_empty() && !output.ends_with('\n') {
            output.push('\n');
        }
        output.push_str(&pi_background_notice(
            canonical.backgrounded_by_user.unwrap_or(false),
            canonical.preempted.unwrap_or(false),
            id,
            hint,
        ));
    }
    if entry.lowering == ToolProjectionLoweringV2::ClaudeBash
        && let Some(id) = canonical.session_id
        && let Some(hint) = canonical.foreground_hint_ms
    {
        if !output.is_empty() && !output.ends_with('\n') {
            output.push('\n');
        }
        let notice = if canonical.backgrounded_by_user.unwrap_or(false) {
            format!("Command was manually backgrounded by user with ID: {id}.")
        } else if canonical.preempted.unwrap_or(false) {
            format!(
                "Command was moved to the background (ID: {id}) so that a message that arrived while it was running can reach you; it was not interrupted."
            )
        } else if hint > 250 {
            let seconds = hint
                .saturating_add(500)
                .checked_div(1000)
                .unwrap_or(1)
                .max(1);
            format!(
                "Command did not complete within its {seconds}s timeout and was moved to the background (ID: {id})."
            )
        } else {
            format!("Command running in background with ID: {id}.")
        };
        output.push_str(&notice);
    }
    if matches!(
        entry.lowering,
        ToolProjectionLoweringV2::GrokRunTerminalCommand
            | ToolProjectionLoweringV2::ClaudeBash
            | ToolProjectionLoweringV2::ClaudeTaskStop
            | ToolProjectionLoweringV2::PiBash
    ) && let Some(trailer) = process_status_trailer(&canonical.status, canonical.exit_code)
    {
        // Vendor models read the text, not the JSON fields: a failed
        // or killed command says so where they look.
        if !output.is_empty() && !output.ends_with('\n') {
            output.push('\n');
        }
        output.push_str(&trailer);
    }
    let raised = serde_json::to_string(&RaisedProcessResultWireV2 {
        exit_code: canonical.exit_code,
        session_id: if entry.lowering == ToolProjectionLoweringV2::GrokRunTerminalCommand {
            None
        } else {
            canonical.session_id
        },
        task_id: if entry.lowering == ToolProjectionLoweringV2::GrokRunTerminalCommand {
            canonical.session_id.map(|id| format!("p{id}"))
        } else {
            None
        },
        status,
        output,
    })
    .map_err(|error| {
        ToolRuntimeError::from_sensitive_source(
            ToolRuntimeErrorKind::Execution,
            "raised process result could not be encoded",
            &error,
        )
    })?;
    entry
        .output_schema
        .parse_instance(&raised)
        .map_err(|error| {
            ToolRuntimeError::from_sensitive_source(
                ToolRuntimeErrorKind::Execution,
                "canonical projection result violates its raised V2 contract",
                &error,
            )
        })?;
    let update = outcome.process_session_update();
    let raised = ToolOutcome::success_text(raised)?;
    Ok(match update {
        Some(update) => raised.with_process_session_update(update),
        None => raised,
    })
}

/// Pi `bash`'s line for a command that keeps running in the background.
fn pi_background_notice(by_user: bool, preempted: bool, id: u64, hint: u64) -> String {
    if by_user {
        format!("Command was moved to the background by the user (id {id}).")
    } else if preempted {
        format!(
            "Command was moved to the background (id {id}) so that a message that arrived while it was running can reach you; it was not interrupted."
        )
    } else if hint > 250 {
        let seconds = (hint.saturating_add(500) / 1000).max(1);
        format!(
            "Command did not finish within its {seconds}s timeout and keeps running in the background (id {id})."
        )
    } else {
        format!("Command running in the background (id {id}).")
    }
}

fn validate_model_content_identity(
    registry: &FrozenToolRegistry,
    identity: &RegisteredToolIdentity,
    outcome: ToolOutcome,
) -> Result<ToolOutcome, ToolRuntimeError> {
    if outcome.kind() != crate::ToolOutcomeKind::Success {
        return Ok(outcome);
    }
    let definition = registry.definition(identity.name()).ok_or_else(|| {
        ToolRuntimeError::new(
            ToolRuntimeErrorKind::Execution,
            "model-content projection source definition is absent",
        )
    })?;
    outcome.validate_contract(definition.output())
}

/// Raises one canonical read result into the presentation's text: Claude's
/// `cat -n` lines or pi's plain text with a continuation line.
fn raise_read_observation(
    entry: &ToolProjectionExposureV2,
    outcome: &ToolOutcome,
) -> Result<ToolOutcome, ToolRuntimeError> {
    let [ToolResultPart::Text(text)] = outcome.parts() else {
        return Err(ToolRuntimeError::new(
            ToolRuntimeErrorKind::Execution,
            "read observation is not one text value",
        ));
    };
    let numbered = if entry.lowering == ToolProjectionLoweringV2::PiRead {
        pi_text_from_canonical_read(text.text())?
    } else {
        claude_cat_n_from_canonical_read(text.text())?
    };
    let update = outcome.process_session_update();
    if entry.raising() == ToolProjectionRaisingV2::TextResultObjectV1 {
        return raise_text_result_object_from_result(&numbered, update);
    }
    // TextBlock forbids empty strings for Identity raising. Direct wraps an empty
    // result above; nested RawUtf8 bypasses this projection and retains neutral JSON.
    let body = if numbered.is_empty() {
        "\n".to_owned()
    } else {
        numbered
    };
    let raised = ToolOutcome::success_text(body)?;
    let raised = match update {
        Some(update) => raised.with_process_session_update(update),
        None => raised,
    };
    if entry.raising() == ToolProjectionRaisingV2::Identity {
        return raise_identity(entry, raised);
    }
    if entry.raising() == ToolProjectionRaisingV2::RawUtf8V1 {
        return Ok(raised);
    }
    Err(ToolRuntimeError::new(
        ToolRuntimeErrorKind::Execution,
        "read observation raising is not enrolled",
    ))
}

/// Pi's `read` text: the selected lines as they are in the file, then the
/// clipping and decoding notes, then where to continue when the page is cut.
fn pi_text_from_canonical_read(canonical: &str) -> Result<String, ToolRuntimeError> {
    let read: crate::WorkspaceFileReadResult =
        serde_json::from_str(canonical).map_err(|error| {
            ToolRuntimeError::from_sensitive_source(
                ToolRuntimeErrorKind::Execution,
                "workspace read observation is not a neutral read result",
                &error,
            )
        })?;
    let mut text = read.content;
    let mut notes = Vec::new();
    if read.clipped_lines != 0 {
        notes.push(format!(
            "[{} lines longer than 2000 characters were clipped]",
            read.clipped_lines
        ));
    }
    if read.lossy {
        notes.push(String::from("[invalid UTF-8 was replaced with U+FFFD]"));
    }
    if let Some(next) = read.next_offset.filter(|_| !read.eof) {
        notes.push(format!(
            "[Showing lines {}-{}. Use offset={next} to continue.]",
            read.offset,
            next.saturating_sub(1)
        ));
    }
    for note in notes {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&note);
    }
    Ok(text)
}

fn claude_cat_n_from_canonical_read(canonical: &str) -> Result<String, ToolRuntimeError> {
    use std::fmt::Write as _;
    let read: crate::WorkspaceFileReadResult =
        serde_json::from_str(canonical).map_err(|error| {
            ToolRuntimeError::from_sensitive_source(
                ToolRuntimeErrorKind::Execution,
                "workspace read observation is not a neutral read result",
                &error,
            )
        })?;
    let body = &read.content;
    let offset = read.offset;
    let eof = read.eof;
    let next_offset = read.next_offset.unwrap_or(0);
    let mut numbered = String::new();
    for (index, line) in body.split_inclusive('\n').enumerate() {
        let line_no = offset.saturating_add(index as u64);
        write!(numbered, "{line_no:>6}\t{line}").map_err(|error| {
            ToolRuntimeError::from_sensitive_source(
                ToolRuntimeErrorKind::Execution,
                "claude read observation could not be numbered",
                &error,
            )
        })?;
    }
    let mut notes = Vec::new();
    if read.clipped_lines != 0 {
        notes.push(format!(
            "... {} lines longer than 2000 characters were clipped",
            read.clipped_lines
        ));
    }
    if read.lossy {
        notes.push(String::from("... invalid UTF-8 was replaced with U+FFFD"));
    }
    for note in notes {
        if !numbered.is_empty() && !numbered.ends_with('\n') {
            numbered.push('\n');
        }
        numbered.push_str(&note);
    }
    if !eof {
        if !numbered.is_empty() && !numbered.ends_with('\n') {
            numbered.push('\n');
        }
        write!(numbered, "... more lines; next_offset={next_offset}").map_err(|error| {
            ToolRuntimeError::from_sensitive_source(
                ToolRuntimeErrorKind::Execution,
                "claude read observation could not be numbered",
                &error,
            )
        })?;
    }
    Ok(numbered)
}

fn raise_identity(
    entry: &ToolProjectionExposureV2,
    outcome: ToolOutcome,
) -> Result<ToolOutcome, ToolRuntimeError> {
    if outcome.kind() == crate::ToolOutcomeKind::Success {
        let [ToolResultPart::Text(text)] = outcome.parts() else {
            return Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::Execution,
                "identity projection result is not one structured text value",
            ));
        };
        entry
            .output_schema
            .parse_instance(text.text())
            .map_err(|error| {
                ToolRuntimeError::from_sensitive_source(
                    ToolRuntimeErrorKind::Execution,
                    "identity projection result violates its raised V2 contract",
                    &error,
                )
            })?;
    }
    Ok(outcome)
}

fn raise_text_result_object(outcome: ToolOutcome) -> Result<ToolOutcome, ToolRuntimeError> {
    if outcome.kind() != crate::ToolOutcomeKind::Success {
        return Ok(outcome);
    }
    let [ToolResultPart::Text(text)] = outcome.parts() else {
        return Err(ToolRuntimeError::new(
            ToolRuntimeErrorKind::Execution,
            "canonical projection result is not one text value",
        ));
    };
    let update = outcome.process_session_update();
    raise_text_result_object_from_result(text.text(), update)
}

/// Whether every `{"result": <string>}` object is an instance of `schema`.
///
/// Construction checks this once, so raising needs no per-result validation:
/// the raised object is bounded by the canonical output contract it wraps, and
/// the runtime's per-result observation cut (not the semantic-JSON string
/// bound) brings it under the model-visible bound.
fn admits_every_text_result(schema: &PortableToolSchemaV2) -> bool {
    let document = schema.document().value();
    document.get("properties") == Some(&serde_json::json!({"result": {"type": "string"}}))
        && document.get("required").is_none_or(|required| {
            required
                .as_array()
                .is_some_and(|required| required.iter().all(|name| name == "result"))
        })
}

fn raise_text_result_object_from_result(
    result: &str,
    update: Option<crate::internal::ProcessSessionUpdate>,
) -> Result<ToolOutcome, ToolRuntimeError> {
    let raised =
        serde_json::to_string(&serde_json::json!({"result": result})).map_err(|error| {
            ToolRuntimeError::from_sensitive_source(
                ToolRuntimeErrorKind::Execution,
                "raised text result could not be encoded",
                &error,
            )
        })?;
    let raised = ToolOutcome::raised_text(raised)?;
    Ok(match update {
        Some(update) => raised.with_process_session_update(update),
        None => raised,
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CanonicalProcessResultWireV2 {
    status: String,
    exit_code: Option<i32>,
    session_id: Option<u64>,
    cursor: u64,
    output: String,
    foreground_hint_ms: Option<u64>,
    preempted: Option<bool>,
    backgrounded_by_user: Option<bool>,
}

impl CanonicalProcessResultWireV2 {
    fn validate(&self) -> Result<(), ToolRuntimeError> {
        let valid = self.cursor < u64::MAX
            && match self.status.as_str() {
                "running" => self.session_id.is_some() && self.exit_code.is_none(),
                "exited" => self.session_id.is_none(),
                "terminated" | "cancelled" | "timed_out" | "output_limit" | "failed"
                | "outcome_unknown" | "cleanup_unresolved" => {
                    self.session_id.is_none() && self.exit_code.is_none()
                }
                _ => false,
            };
        if !valid {
            return Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::Execution,
                "canonical process result status is inconsistent",
            ));
        }
        Ok(())
    }
}

#[derive(Serialize)]
struct RaisedProcessResultWireV2 {
    #[serde(skip_serializing_if = "Option::is_none")]
    exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    task_id: Option<String>,
    /// Set only when the process did not run to its own exit.
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<&'static str>,
    output: String,
}

/// The raised status for a process that ended other than by its own exit.
fn raised_process_status(status: &str) -> Option<&'static str> {
    match status {
        "timed_out" => Some("timed_out"),
        "terminated" => Some("terminated"),
        "cancelled" => Some("cancelled"),
        "output_limit" => Some("output_limit"),
        "failed" => Some("failed"),
        "outcome_unknown" => Some("outcome_unknown"),
        "cleanup_unresolved" => Some("cleanup_unresolved"),
        _ => None,
    }
}

/// The one-line text trailer for a nonzero exit or an abnormal end.
fn process_status_trailer(status: &str, exit_code: Option<i32>) -> Option<String> {
    match status {
        "exited" => exit_code
            .filter(|code| *code != 0)
            .map(|code| format!("[exit {code}]")),
        "timed_out" => Some(String::from("[timed out; the process group was killed]")),
        "terminated" => Some(String::from("[killed]")),
        "cancelled" => Some(String::from("[cancelled]")),
        "output_limit" => Some(String::from("[killed: output limit]")),
        "failed" => Some(String::from(
            "[failed: the process could not be supervised]",
        )),
        "outcome_unknown" => Some(String::from(
            "[outcome unknown: the previous process owner is unavailable]",
        )),
        "cleanup_unresolved" => Some(String::from("[cleanup unresolved]")),
        _ => None,
    }
}

fn compiled_projection_error(error: &plakous_model::RequestError) -> ToolRuntimeError {
    ToolRuntimeError::from_sensitive_source(
        ToolRuntimeErrorKind::Registration,
        "model request projection view rejected the exact tools-owned map",
        error,
    )
}

impl fmt::Debug for ToolProjectionMapV2 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ToolProjectionMapV2")
            .field("target", &self.target)
            .field("entry_count", &self.entries.len())
            .field("registry_digest", &self.registry_digest)
            .field("digest", &self.digest)
            .finish_non_exhaustive()
    }
}

fn validate_projection_defaults(
    schema: &PortableToolSchemaV2,
    defaults: &Map<String, Value>,
) -> Result<(), ToolRuntimeError> {
    let root = schema
        .document()
        .value()
        .as_object()
        .ok_or_else(|| registration_error("projection input schema root is not an object"))?;
    let properties = root
        .get("properties")
        .and_then(Value::as_object)
        .ok_or_else(|| registration_error("projection input properties are missing"))?;
    let required = root
        .get("required")
        .and_then(Value::as_array)
        .ok_or_else(|| registration_error("projection required set is missing"))?;
    for (name, value) in defaults {
        if !properties.contains_key(name)
            || required
                .iter()
                .any(|required| required.as_str() == Some(name))
        {
            return Err(registration_error(
                "projection defaults must name only optional declared properties",
            ));
        }
        let instance = SemanticJsonV1::from_value(Value::Object(Map::from_iter([(
            name.clone(),
            value.clone(),
        )])))
        .map_err(|error| {
            ToolRuntimeError::from_sensitive_source(
                ToolRuntimeErrorKind::Registration,
                "projection default is not semantic JSON",
                &error,
            )
        })?;
        let _ = instance;
    }
    let mut complete = defaults.clone();
    for required_name in required.iter().filter_map(Value::as_str) {
        complete.insert(
            required_name.to_owned(),
            placeholder_for(&properties[required_name])?,
        );
    }
    schema
        .parse_instance(&Value::Object(complete).to_string())
        .map_err(|error| {
            ToolRuntimeError::from_sensitive_source(
                ToolRuntimeErrorKind::Registration,
                "projection defaults violate the projected V2 schema",
                &error,
            )
        })?;
    Ok(())
}

fn placeholder_for(schema: &Value) -> Result<Value, ToolRuntimeError> {
    if let Some(value) = schema
        .get("enum")
        .and_then(Value::as_array)
        .and_then(|values| values.first())
    {
        return Ok(value.clone());
    }
    match schema.get("type").and_then(Value::as_str) {
        Some("string") => Ok(Value::String(
            "x".repeat(
                usize::try_from(schema.get("minLength").and_then(Value::as_u64).unwrap_or(1))
                    .map_err(|_| registration_error("projection string bound is not portable"))?,
            ),
        )),
        Some("integer" | "number") => Ok(schema.get("minimum").cloned().unwrap_or(Value::from(0))),
        Some("boolean") => Ok(Value::Bool(false)),
        Some("array") => {
            let min_items = schema
                .get("minItems")
                .and_then(json_positive_u64)
                .unwrap_or(0);
            if min_items == 0 {
                return Ok(Value::Array(Vec::new()));
            }
            let count = usize::try_from(min_items)
                .map_err(|_| registration_error("projection array bound is not portable"))?;
            let items = schema
                .get("items")
                .ok_or_else(|| registration_error("projection schema type is unsupported"))?;
            let element = placeholder_for(items)?;
            Ok(Value::Array(vec![element; count]))
        }
        Some("object") => {
            let mut object = Map::new();
            for name in schema
                .get("required")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
            {
                let property = schema
                    .get("properties")
                    .and_then(|properties| properties.get(name))
                    .ok_or_else(|| registration_error("projection schema type is unsupported"))?;
                object.insert(name.to_owned(), placeholder_for(property)?);
            }
            Ok(Value::Object(object))
        }
        _ => Err(registration_error("projection schema type is unsupported")),
    }
}

fn json_u64(value: &Value) -> Option<u64> {
    if let Some(number) = value.as_u64() {
        return Some(number);
    }
    let raw = match value {
        Value::Number(number) => number.to_string(),
        Value::String(text) => text.clone(),
        _ => return None,
    };
    raw.parse::<u64>().ok().or_else(|| {
        raw.trim_end_matches('0')
            .trim_end_matches('.')
            .parse::<u64>()
            .ok()
    })
}

fn json_positive_u64(value: &Value) -> Option<u64> {
    json_u64(value).filter(|number| *number >= 1)
}

/// Reference unified exec clamps every `exec_command` start and poll yield to
/// this window. A model-sent `yield_time_ms: 0` must not observe an empty pipe
/// and report a still-running process as the whole result.
pub const UNIFIED_EXEC_MIN_YIELD_MS: u64 = 250;
/// Upper bound of one unified exec observation; longer requests are clamped.
pub const UNIFIED_EXEC_MAX_YIELD_MS: u64 = 600_000;
const UNIFIED_EXEC_DEFAULT_YIELD_MS: u64 = 10_000;

/// Reference `write_stdin` waits briefly for the output its input provokes.
const WRITE_STDIN_DEFAULT_YIELD_MS: u64 = 250;

fn unified_exec_max_output_bytes(arguments: &Map<String, Value>) -> u64 {
    arguments
        .get("max_output_tokens")
        .and_then(json_u64)
        .and_then(|tokens| tokens.checked_mul(4))
        .unwrap_or(PROCESS_OBSERVATION_BYTES as u64)
        .clamp(1, PROCESS_OBSERVATION_BYTES as u64)
}

fn unified_exec_yield_time_ms(arguments: &Map<String, Value>) -> u64 {
    arguments
        .get("yield_time_ms")
        .and_then(json_u64)
        .unwrap_or(UNIFIED_EXEC_DEFAULT_YIELD_MS)
        .clamp(UNIFIED_EXEC_MIN_YIELD_MS, UNIFIED_EXEC_MAX_YIELD_MS)
}

/// Claude Code's own `Bash` timeout ceiling.
const CLAUDE_BASH_MAX_TIMEOUT_MS: u64 = 600_000;
/// All start vocabularies share the foreground hint ceiling.
const GROK_TERMINAL_MAX_TIMEOUT_MS: u64 = UNIFIED_EXEC_MAX_YIELD_MS;

/// Pi's typed refusal of an image path: pi 1.1.0's `read` here is text only.
pub const PI_READ_IMAGE_REFUSAL: &str =
    "read returns text only; image files (png, jpg, jpeg, gif, webp, bmp) are not supported";

/// Whether a path names one of the image formats pi's `read` would attach.
fn is_image_path(path: &str) -> bool {
    Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            ["png", "jpg", "jpeg", "gif", "webp", "bmp"]
                .iter()
                .any(|image| extension.eq_ignore_ascii_case(image))
        })
}

/// Pi's `wait` without `timeout_ms` awaits as long as `wait_agent` does.
const PI_WAIT_DEFAULT_TIMEOUT_MS: u64 = 300_000;
/// Every await presentation clamps to one hour.
const PI_WAIT_MAX_TIMEOUT_MS: u64 = 3_600_000;

/// A pi process id as the model writes it: `N` or `pN`, nonzero. Anything
/// else is refused before any effect, without claiming to know what it names:
/// pi's `wait` and `stop` cover processes; agent tools cover agents.
fn pi_process_id(value: &Value) -> Result<u64, ToolRuntimeError> {
    let text = value.as_str().ok_or_else(|| {
        ToolRuntimeError::new(
            ToolRuntimeErrorKind::InvalidArguments,
            "ids must be strings",
        )
    })?;
    text.strip_prefix('p')
        .unwrap_or(text)
        .parse::<u32>()
        .ok()
        .filter(|number| *number != 0)
        .map(u64::from)
        .ok_or_else(|| {
            ToolRuntimeError::new(
                ToolRuntimeErrorKind::Policy,
                format!("id `{text}` is not a process id; this agent has no agent tools"),
            )
        })
}

/// Claude addresses a background shell by the string form of its `session_id`.
fn process_task_id(value: &Value, field: &str) -> Result<u64, ToolRuntimeError> {
    value.as_str()
        .and_then(process_task_number)
        .ok_or_else(|| ToolRuntimeError::new(
            ToolRuntimeErrorKind::InvalidArguments,
            format!("{field} must contain process ids returned by run_terminal_command (N or pN); agent ids are not accepted"),
        ))
}

/// A process task id as the model writes it: `N` or `pN`, nonzero.
fn process_task_number(text: &str) -> Option<u64> {
    text.strip_prefix('p')
        .unwrap_or(text)
        .parse::<u32>()
        .ok()
        .filter(|number| *number != 0)
        .map(u64::from)
}

/// Grok's output and kill tools as presented to a node without agent tools:
/// anything that is not a process id is refused before any effect, without
/// claiming to know what else it names.
fn grok_process_task_id(value: &Value) -> Result<u64, ToolRuntimeError> {
    let text = value.as_str().ok_or_else(|| {
        ToolRuntimeError::new(
            ToolRuntimeErrorKind::InvalidArguments,
            "task ids must be strings",
        )
    })?;
    process_task_number(text).ok_or_else(|| {
        ToolRuntimeError::new(
            ToolRuntimeErrorKind::Policy,
            format!("task id `{text}` is not a process id; this agent has no agent tools"),
        )
    })
}

fn capability_operation(
    entry: &ToolProjectionExposureV2,
) -> Result<CanonicalProcessOperationV2, ToolRuntimeError> {
    entry
        .canonical_operations
        .keys()
        .copied()
        .next()
        .ok_or_else(|| registration_error("projection capability binding is empty"))
}

// Keep the canonical input contract's wire shape after lowering. A UTF-8
// projection is raw text, while structured projections are semantic JSON.
enum LoweredProjectionArguments {
    Utf8(String),
    Structured(SemanticJsonV1),
}

impl LoweredProjectionArguments {
    fn into_request_arguments(self) -> Result<String, ToolRuntimeError> {
        match self {
            Self::Utf8(text) => Ok(text),
            Self::Structured(json) => std::str::from_utf8(json.canonical_bytes())
                .map(str::to_owned)
                .map_err(|_| registration_error("canonical projection arguments are not UTF-8")),
        }
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "closed lowering programs stay in one audited match"
)]
fn lower_projection(
    entry: &ToolProjectionExposureV2,
    projected: &Value,
    environment: Option<&ExecutionEnvironment>,
) -> Result<(CanonicalProcessOperationV2, LoweredProjectionArguments), ToolRuntimeError> {
    if entry.lowering == ToolProjectionLoweringV2::CodexApplyPatch {
        let patch = match projected {
            Value::String(text) => text.clone(),
            Value::Object(object) => object
                .get("patch")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| {
                    ToolRuntimeError::new(
                        ToolRuntimeErrorKind::InvalidArguments,
                        "apply_patch requires patch envelope text",
                    )
                })?,
            _ => {
                return Err(ToolRuntimeError::new(
                    ToolRuntimeErrorKind::InvalidArguments,
                    "apply_patch input must be patch envelope text",
                ));
            }
        };
        return Ok((
            capability_operation(entry)?,
            LoweredProjectionArguments::Utf8(patch),
        ));
    }
    if matches!(
        entry.presentation(),
        ProjectedToolPresentationV2::CustomUtf8 { .. }
    ) {
        let Value::String(text) = projected else {
            return Err(registration_error(
                "custom projection did not validate as UTF-8 text",
            ));
        };
        if entry.lowering != ToolProjectionLoweringV2::CanonicalPassthrough {
            return Err(registration_error(
                "custom projection has no UTF-8 lowering",
            ));
        }
        return Ok((
            capability_operation(entry)?,
            LoweredProjectionArguments::Utf8(text.clone()),
        ));
    }
    let mut arguments = entry
        .defaults
        .value()
        .as_object()
        .cloned()
        .ok_or_else(|| registration_error("projection defaults are not an object"))?;
    for (name, value) in projected
        .as_object()
        .ok_or_else(|| registration_error("projected input is not an object"))?
    {
        arguments.insert(name.clone(), value.clone());
    }
    let (operation, mut canonical) = match entry.lowering {
        ToolProjectionLoweringV2::CodexExecCommand => {
            let max_output_bytes = unified_exec_max_output_bytes(&arguments);
            let cmd = arguments
                .get("cmd")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|command| !command.is_empty());
            // Official unified-exec: cmd starts a process. session_id without
            // cmd polls. Models often send a hallucinated session_id with cmd;
            // treating that as Poll loops on "unknown or stale" forever.
            if let Some(command) = cmd {
                (
                    CanonicalProcessOperationV2::Start,
                    json!({
                        "command": command,
                        "workdir": arguments.get("workdir").cloned().unwrap_or_else(|| Value::String(String::from("."))),
                        "login": arguments.get("login").cloned().unwrap_or(Value::Bool(true)),
                        "tty": arguments.get("tty").cloned().unwrap_or(Value::Bool(false)),
                        "keep_stdin_open": arguments.get("tty").cloned().unwrap_or(Value::Bool(false)),
                        "yield_time_ms": if projected.get("yield_time_ms").is_some() {
                            unified_exec_yield_time_ms(&arguments).min(600_000)
                        } else {
                            environment.map_or(30_000, |environment| environment.config().limits().foreground_seconds() * 1000)
                        },
                        "max_output_bytes": max_output_bytes,
                    }),
                )
            } else if arguments
                .get("session_id")
                .and_then(json_positive_u64)
                .is_some()
            {
                (
                    CanonicalProcessOperationV2::Poll,
                    json!({
                        "session_id": arguments.get("session_id").cloned().unwrap_or(Value::Null),
                        "yield_time_ms": unified_exec_yield_time_ms(&arguments),
                        "max_output_bytes": max_output_bytes,
                        "max_output_records": 256,
                    }),
                )
            } else {
                return Err(ToolRuntimeError::new(
                    ToolRuntimeErrorKind::InvalidArguments,
                    "exec_command requires cmd to start, or session_id to read more output",
                ));
            }
        }
        ToolProjectionLoweringV2::CodexWriteStdin => {
            let empty = arguments
                .get("chars")
                .and_then(Value::as_str)
                .is_none_or(str::is_empty);
            let yield_time_ms = arguments
                .get("yield_time_ms")
                .and_then(json_u64)
                .unwrap_or(if empty {
                    5_000
                } else {
                    WRITE_STDIN_DEFAULT_YIELD_MS
                })
                .clamp(
                    if empty {
                        5_000
                    } else {
                        UNIFIED_EXEC_MIN_YIELD_MS
                    },
                    if empty { 300_000 } else { 30_000 },
                );
            (
                CanonicalProcessOperationV2::Write,
                json!({
                    "session_id": arguments.get("session_id").cloned().unwrap_or(Value::Null),
                    "input": arguments.get("chars").cloned().unwrap_or_else(|| Value::String(String::new())),
                    "disposition": "keep_open",
                    "yield_time_ms": yield_time_ms,
                    "max_output_bytes": unified_exec_max_output_bytes(&arguments),
                }),
            )
        }
        ToolProjectionLoweringV2::GrokRunTerminalCommand | ToolProjectionLoweringV2::ClaudeBash => {
            let claude = matches!(entry.lowering, ToolProjectionLoweringV2::ClaudeBash);
            let background = claude
                && arguments
                    .get("run_in_background")
                    .and_then(Value::as_bool)
                    .is_some_and(|background| background);
            let page = if claude {
                CLAUDE_BASH_OBSERVATION_BYTES
            } else {
                PROCESS_OBSERVATION_BYTES
            };
            let timeout = arguments
                .get(if claude { "timeout" } else { "block_until_ms" })
                .and_then(json_u64)
                .unwrap_or_else(|| {
                    environment.map_or(30_000, |environment| {
                        environment.config().limits().foreground_seconds() * 1000
                    })
                })
                .clamp(
                    UNIFIED_EXEC_MIN_YIELD_MS,
                    if claude {
                        CLAUDE_BASH_MAX_TIMEOUT_MS
                    } else {
                        GROK_TERMINAL_MAX_TIMEOUT_MS
                    },
                );
            let mut start = json!({
                "command": arguments.get("command").cloned().unwrap_or(Value::Null),
                "workdir": ".",
                "login": false,
                "tty": false,
                "keep_stdin_open": false,
                // A Claude background start returns the first moment of
                // output and its session; the rest arrives in the completion
                // notice and, when one was issued, the output file `Read` reads.
                "yield_time_ms": if background { UNIFIED_EXEC_MIN_YIELD_MS } else { timeout },
                "max_output_bytes": page,
            });
            if let Some(description) = arguments
                .get("description")
                .cloned()
                .filter(|value| !value.is_null())
            {
                start["model_description"] = description;
            }
            (CanonicalProcessOperationV2::Start, start)
        }
        ToolProjectionLoweringV2::GrokGetCommandOutput => {
            let ids = arguments.get("task_ids").and_then(Value::as_array);
            let session_ids = ids
                .into_iter()
                .flatten()
                .map(grok_process_task_id)
                .collect::<Result<Vec<_>, _>>()?;
            (
                CanonicalProcessOperationV2::Poll,
                json!({
                    "session_ids": session_ids,
                    "yield_time_ms": arguments.get("timeout_ms").and_then(json_u64).unwrap_or(0).min(3_600_000),
                    "max_output_bytes": PROCESS_OBSERVATION_BYTES,
                    "max_output_records": 256,
                }),
            )
        }
        ToolProjectionLoweringV2::PiBash => {
            // `timeout` is seconds and only an observation bound: omitted is
            // the bound foreground budget, 0 starts in the background.
            let yield_time_ms = match arguments.get("timeout").and_then(json_u64) {
                Some(0) => UNIFIED_EXEC_MIN_YIELD_MS,
                Some(seconds) => seconds
                    .saturating_mul(1000)
                    .clamp(UNIFIED_EXEC_MIN_YIELD_MS, UNIFIED_EXEC_MAX_YIELD_MS),
                None => environment.map_or(30_000, |environment| {
                    environment.config().limits().foreground_seconds() * 1000
                }),
            };
            (
                CanonicalProcessOperationV2::Start,
                json!({
                    "command": arguments.get("command").cloned().unwrap_or(Value::Null),
                    "workdir": ".",
                    "login": false,
                    "tty": false,
                    "keep_stdin_open": false,
                    "yield_time_ms": yield_time_ms,
                    "max_output_bytes": PROCESS_OBSERVATION_BYTES,
                }),
            )
        }
        ToolProjectionLoweringV2::PiWait => {
            let mut wait = json!({
                "wake": "any",
                "yield_time_ms": arguments
                    .get("timeout_ms")
                    .and_then(json_u64)
                    .unwrap_or(PI_WAIT_DEFAULT_TIMEOUT_MS)
                    .min(PI_WAIT_MAX_TIMEOUT_MS),
                "max_output_bytes": PROCESS_OBSERVATION_BYTES,
                "max_output_records": 256,
            });
            if let Some(ids) = arguments.get("ids").and_then(Value::as_array) {
                wait["session_ids"] = ids
                    .iter()
                    .map(pi_process_id)
                    .collect::<Result<Vec<_>, _>>()?
                    .into();
            }
            (CanonicalProcessOperationV2::Poll, wait)
        }
        ToolProjectionLoweringV2::PiStop => (
            CanonicalProcessOperationV2::Terminate,
            json!({"session_id": pi_process_id(arguments.get("id").unwrap_or(&Value::Null))?}),
        ),
        ToolProjectionLoweringV2::PiRead => {
            let path = arguments
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if is_image_path(path) {
                return Err(ToolRuntimeError::new(
                    ToolRuntimeErrorKind::InvalidArguments,
                    PI_READ_IMAGE_REFUSAL,
                ));
            }
            let mut read = Map::new();
            read.insert(
                "path".to_owned(),
                arguments.get("path").cloned().unwrap_or(Value::Null),
            );
            if let Some(offset) = arguments
                .get("offset")
                .and_then(json_positive_u64)
                .map(Value::from)
            {
                read.insert("offset".to_owned(), offset);
            }
            // The canonical page holds at most 2000 lines; a larger `limit`
            // reads that page and says where to continue.
            let limit = arguments
                .get("limit")
                .and_then(json_positive_u64)
                .unwrap_or(2000)
                .min(2000);
            read.insert("limit".to_owned(), Value::from(limit));
            (capability_operation(entry)?, Value::Object(read))
        }
        ToolProjectionLoweringV2::PiEdit => {
            let edits = arguments
                .get("edits")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(|edit| {
                    let old_text = edit.get("oldText").cloned().unwrap_or(Value::Null);
                    if old_text.as_str().is_some_and(str::is_empty) {
                        return Err(ToolRuntimeError::new(
                            ToolRuntimeErrorKind::InvalidArguments,
                            "edit requires a non-empty edits[].oldText; use write to create a file",
                        ));
                    }
                    Ok(json!({
                        "old_string": old_text,
                        "new_string": edit.get("newText").cloned().unwrap_or(Value::Null),
                    }))
                })
                .collect::<Result<Vec<_>, _>>()?;
            (
                capability_operation(entry)?,
                json!({
                    "file_path": arguments.get("path").cloned().unwrap_or(Value::Null),
                    "edits": edits,
                }),
            )
        }
        ToolProjectionLoweringV2::PiWrite => (
            capability_operation(entry)?,
            json!({
                "file_path": arguments.get("path").cloned().unwrap_or(Value::Null),
                "old_string": "",
                "new_string": arguments.get("content").cloned().unwrap_or(Value::Null),
                "replace_all": true,
            }),
        ),
        ToolProjectionLoweringV2::GrokKillCommand => (
            CanonicalProcessOperationV2::Terminate,
            json!({"session_id": grok_process_task_id(arguments.get("task_id").unwrap_or(&Value::Null))?}),
        ),
        ToolProjectionLoweringV2::ClaudeTaskStop => (
            CanonicalProcessOperationV2::Terminate,
            json!({"session_id": process_task_id(arguments.get("task_id").unwrap_or(&Value::Null), "task_id")?}),
        ),
        ToolProjectionLoweringV2::CanonicalPassthrough => (
            capability_operation(entry)?,
            Value::Object(arguments.clone()),
        ),
        ToolProjectionLoweringV2::CodexApplyPatch => {
            return Err(registration_error(
                "CodexApplyPatch lowering must be handled before structured merge",
            ));
        }
        ToolProjectionLoweringV2::GrokReadFile => {
            let mut read = Map::new();
            read.insert(
                "path".to_owned(),
                arguments.get("target_file").cloned().unwrap_or(Value::Null),
            );
            let offset = arguments
                .get("offset")
                .cloned()
                .filter(|value| !value.is_null())
                .unwrap_or(json!(1));
            read.insert("offset".to_owned(), offset);
            let limit = arguments
                .get("limit")
                .cloned()
                .filter(|value| !value.is_null())
                .unwrap_or(json!(1000));
            read.insert("limit".to_owned(), limit);
            (capability_operation(entry)?, Value::Object(read))
        }
        ToolProjectionLoweringV2::GrokGrep => {
            if arguments
                .get("multiline")
                .and_then(Value::as_bool)
                .is_some_and(|enabled| enabled)
            {
                return Err(ToolRuntimeError::new(
                    ToolRuntimeErrorKind::InvalidArguments,
                    "grep multiline mode is unsupported",
                ));
            }
            let mut search = Map::new();
            search.insert(
                "query".to_owned(),
                arguments.get("pattern").cloned().unwrap_or(Value::Null),
            );
            search.insert(
                "path".to_owned(),
                arguments
                    .get("path")
                    .cloned()
                    .filter(|value| !value.is_null())
                    .unwrap_or_else(|| Value::String(String::from("."))),
            );
            let case_sensitive = arguments
                .get("-i")
                .and_then(Value::as_bool)
                .is_some_and(|insensitive| !insensitive);
            search.insert("case_sensitive".to_owned(), Value::Bool(case_sensitive));
            search.insert("use_regex".to_owned(), Value::Bool(true));
            if let Some(glob) = arguments
                .get("glob")
                .cloned()
                .filter(|value| !value.is_null())
            {
                search.insert("filename_glob".to_owned(), glob);
            } else if let Some(file_type) = arguments
                .get("type")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
            {
                search.insert("filename_glob".to_owned(), grok_type_to_glob(file_type)?);
            }
            if let Some(head_limit) = arguments
                .get("head_limit")
                .and_then(json_positive_u64)
                .map(Value::from)
            {
                search.insert("head_limit".to_owned(), head_limit);
            } else {
                search.insert("head_limit".to_owned(), json!(200));
            }
            for (arg, field) in [
                ("-B", "before_context"),
                ("-A", "after_context"),
                ("-C", "context"),
            ] {
                if let Some(lines) = arguments.get(arg).and_then(json_positive_u64) {
                    search.insert(field.to_owned(), Value::from(lines));
                }
            }
            (capability_operation(entry)?, Value::Object(search))
        }
        ToolProjectionLoweringV2::GrokSearchReplace | ToolProjectionLoweringV2::ClaudeEdit => {
            let old_string = arguments.get("old_string").cloned().unwrap_or(Value::Null);
            if entry.lowering == ToolProjectionLoweringV2::ClaudeEdit
                && old_string.as_str().is_some_and(str::is_empty)
            {
                return Err(ToolRuntimeError::new(
                    ToolRuntimeErrorKind::InvalidArguments,
                    "Edit requires a non-empty old_string; use Write to create a new file",
                ));
            }
            let replace_all = if old_string.as_str() == Some("") {
                Value::Bool(false)
            } else {
                arguments
                    .get("replace_all")
                    .cloned()
                    .unwrap_or(Value::Bool(false))
            };
            (
                capability_operation(entry)?,
                json!({
                    "file_path": arguments.get("file_path").cloned().unwrap_or(Value::Null),
                    "old_string": old_string,
                    "new_string": arguments.get("new_string").cloned().unwrap_or(Value::Null),
                    "replace_all": replace_all,
                }),
            )
        }
        // Claude Code's Write creates or overwrites, like Grok's `write`.
        ToolProjectionLoweringV2::GrokWrite | ToolProjectionLoweringV2::ClaudeWrite => (
            capability_operation(entry)?,
            json!({
                "file_path": arguments.get("file_path").cloned().unwrap_or(Value::Null),
                "old_string": "",
                "new_string": arguments.get("content").cloned().unwrap_or(Value::Null),
                "replace_all": true,
            }),
        ),
        ToolProjectionLoweringV2::GrokListDir => (
            capability_operation(entry)?,
            json!({
                "path": arguments.get("target_directory").cloned().unwrap_or(Value::Null),
                // `search_repository` requires `query` even when it only
                // lists: a listing carries the empty needle.
                "query": "",
                "list_directory": true,
            }),
        ),
        ToolProjectionLoweringV2::ClaudeReadFile => {
            let mut read = Map::new();
            read.insert(
                "path".to_owned(),
                arguments.get("file_path").cloned().unwrap_or(Value::Null),
            );
            if let Some(offset) = arguments
                .get("offset")
                .and_then(json_positive_u64)
                .map(Value::from)
            {
                read.insert("offset".to_owned(), offset);
            }
            let limit = arguments
                .get("limit")
                .cloned()
                .filter(|value| !value.is_null())
                .unwrap_or(json!(2000));
            read.insert("limit".to_owned(), limit);
            (capability_operation(entry)?, Value::Object(read))
        }
    };
    if operation == CanonicalProcessOperationV2::Start
        && matches!(
            entry.lowering,
            ToolProjectionLoweringV2::CodexExecCommand
                | ToolProjectionLoweringV2::GrokRunTerminalCommand
                | ToolProjectionLoweringV2::ClaudeBash
                | ToolProjectionLoweringV2::PiBash
        )
    {
        canonical["hint_source"] = Value::String(
            if arguments.get("run_in_background").and_then(Value::as_bool) == Some(true)
                || (entry.lowering == ToolProjectionLoweringV2::PiBash
                    && arguments.get("timeout").and_then(json_u64) == Some(0))
            {
                "background"
            } else if ["yield_time_ms", "block_until_ms", "timeout"]
                .iter()
                .any(|key| projected.get(*key).is_some())
            {
                "model_hint"
            } else {
                "default"
            }
            .into(),
        );
    }
    if !entry.canonical_operations.contains_key(&operation) {
        return Err(registration_error(
            "projection lowering selected an unenrolled canonical operation",
        ));
    }
    if entry.lowering == ToolProjectionLoweringV2::CodexExecCommand
        && operation == CanonicalProcessOperationV2::Start
        && let Some(environment) = environment
    {
        let workdir = canonical
            .get("workdir")
            .and_then(Value::as_str)
            .ok_or_else(projection_workdir_error)?;
        canonical["workdir"] = Value::String(
            canonical_process_workdir(environment, Path::new(workdir))?
                .to_string_lossy()
                .into_owned(),
        );
    }
    let canonical = SemanticJsonV1::from_value(canonical).map_err(|error| {
        ToolRuntimeError::from_sensitive_source(
            ToolRuntimeErrorKind::InvalidArguments,
            "projected input could not be lowered canonically",
            &error,
        )
    })?;
    Ok((operation, LoweredProjectionArguments::Structured(canonical)))
}

fn canonical_process_workdir(
    environment: &ExecutionEnvironment,
    workdir: &Path,
) -> Result<PathBuf, ToolRuntimeError> {
    let relative = if workdir.is_absolute() {
        workdir
            .strip_prefix(environment.cwd())
            .map_err(|_| projection_workdir_error())?
    } else {
        workdir
    };
    if relative
        .components()
        .any(|component| !matches!(component, Component::Normal(_) | Component::CurDir))
    {
        return Err(projection_workdir_error());
    }
    if relative.as_os_str().is_empty() {
        Ok(PathBuf::from("."))
    } else {
        Ok(relative.to_path_buf())
    }
}

fn projection_workdir_error() -> ToolRuntimeError {
    ToolRuntimeError::new(
        ToolRuntimeErrorKind::WorkspacePolicy,
        "process working directory must be the active workspace cwd or one of its descendants",
    )
}

fn projection_program_identity(
    direction: &[u8],
    program: &[u8],
    data: Digest,
) -> Result<Digest, ToolRuntimeError> {
    Digest::sha256(
        DigestDomain::ToolDefinition,
        2,
        &[
            b"plakous-tools/projection-program-v2",
            direction,
            program,
            data.bytes(),
        ],
    )
    .map_err(|error| {
        ToolRuntimeError::from_sensitive_source(
            ToolRuntimeErrorKind::Registration,
            "projection program identity could not be constructed",
            &error,
        )
    })
}

#[allow(
    clippy::too_many_arguments,
    reason = "the digest binds the complete descriptor"
)]
fn projection_entry_identity(
    canonical: &BTreeMap<CanonicalProcessOperationV2, RegisteredToolIdentity>,
    exposed: &ToolName,
    description: &str,
    presentation: &ProjectedToolPresentationV2,
    lowering: Digest,
    output: &PortableToolSchemaV2,
    raising: Digest,
    target: &ProviderModelProjectionIdentityV2,
) -> Result<Digest, ToolRuntimeError> {
    let target = serde_json::to_vec(target).map_err(|error| {
        ToolRuntimeError::from_sensitive_source(
            ToolRuntimeErrorKind::Registration,
            "projection target could not be encoded",
            &error,
        )
    })?;
    let presentation = serde_json::to_vec(presentation).map_err(|error| {
        ToolRuntimeError::from_sensitive_source(
            ToolRuntimeErrorKind::Registration,
            "projection presentation could not be encoded",
            &error,
        )
    })?;
    let mut operation_bytes = Vec::new();
    for (operation, identity) in canonical {
        operation_bytes.extend_from_slice(
            format!(
                "{operation:?}\0{}\0{}\0{}\0",
                identity.tool_id(),
                identity.semantic_version(),
                identity.executor_id()
            )
            .as_bytes(),
        );
    }
    Digest::sha256(
        DigestDomain::ToolDefinition,
        3,
        &[
            b"plakous-tools/projection-entry-v3",
            &operation_bytes,
            exposed.as_str().as_bytes(),
            description.as_bytes(),
            &presentation,
            lowering.bytes(),
            output.digest().bytes(),
            raising.bytes(),
            &target,
        ],
    )
    .map_err(|error| {
        ToolRuntimeError::from_sensitive_source(
            ToolRuntimeErrorKind::Registration,
            "projection entry identity could not be constructed",
            &error,
        )
    })
}

pub(crate) fn projection_map_digest(
    registry: Digest,
    target: &ProviderModelProjectionIdentityV2,
    entries: &[ToolProjectionExposureV2],
) -> Result<Digest, ToolRuntimeError> {
    let target = serde_json::to_vec(target).map_err(|error| {
        ToolRuntimeError::from_sensitive_source(
            ToolRuntimeErrorKind::Registration,
            "projection target could not be encoded",
            &error,
        )
    })?;
    let mut ordered = entries.iter().collect::<Vec<_>>();
    ordered.sort_by(|left, right| left.exposed_name().cmp(right.exposed_name()));
    let mut parts = vec![
        b"plakous-tools/projection-map-v3".as_slice(),
        registry.bytes(),
        &target,
    ];
    parts.extend(
        ordered
            .iter()
            .map(|entry| entry.identity.bytes().as_slice()),
    );
    Digest::sha256(
        DigestDomain::ToolDefinition,
        PROJECTION_MAP_DIGEST_VERSION,
        &parts,
    )
    .map_err(|error| {
        ToolRuntimeError::from_sensitive_source(
            ToolRuntimeErrorKind::Registration,
            "projection map identity could not be constructed",
            &error,
        )
    })
}

pub(crate) fn exposed_definition(
    canonical: &ToolDefinition,
    exposure: &ToolExposure,
) -> Result<ToolDefinition, ToolRuntimeError> {
    ToolDefinition::new(
        exposure.exposed_name.clone(),
        canonical.semantic_version(),
        exposure
            .description_override()
            .unwrap_or_else(|| canonical.description()),
        canonical.input().clone(),
        canonical.output().clone(),
        canonical.concurrency(),
        canonical.effect(),
    )
    .map_err(|error| {
        ToolRuntimeError::from_sensitive_source(
            ToolRuntimeErrorKind::InvalidDefinition,
            "exposed tool definition could not be constructed",
            &error,
        )
    })
}

pub(crate) fn registry_identity_digest(
    registry: &FrozenToolRegistry,
) -> Result<Digest, ToolRuntimeError> {
    let identities = registry.identities().cloned().collect::<Vec<_>>();
    registry_identity_digest_from_identities(&identities)
}

pub(crate) fn registry_identity_digest_from_identities(
    identities: &[RegisteredToolIdentity],
) -> Result<Digest, ToolRuntimeError> {
    let mut identities = identities.iter().collect::<Vec<_>>();
    identities.sort_by(|left, right| left.name().cmp(right.name()));
    let mut parts = vec![b"plakous-tools/frozen-registry".to_vec()];
    for identity in identities {
        push_identity_parts(&mut parts, identity);
    }
    digest_parts(
        DigestDomain::ToolDefinition,
        REGISTRY_IDENTITY_DIGEST_VERSION,
        &parts,
    )
}

pub(crate) fn exposure_map_digest(
    entries: &[ToolExposure],
    definitions: &[ToolDefinition],
) -> Result<Digest, ToolRuntimeError> {
    if entries.len() != definitions.len() {
        return Err(registration_error(
            "tool exposure identity inputs are not exactly aligned",
        ));
    }
    let mut aligned = entries.iter().zip(definitions).collect::<Vec<_>>();
    aligned.sort_by(|(left, _), (right, _)| {
        left.canonical_identity
            .name()
            .cmp(right.canonical_identity.name())
    });
    let mut parts = vec![b"plakous-tools/tool-exposure-map".to_vec()];
    for (exposure, definition) in aligned {
        push_identity_parts(&mut parts, &exposure.canonical_identity);
        parts.push(exposure.exposed_name.as_str().as_bytes().to_vec());
        parts.push(b"exposed-definition-digest/sha-256/tool-definition".to_vec());
        parts.push(definition.digest().version().to_be_bytes().to_vec());
        parts.push(definition.digest().bytes().to_vec());
    }
    digest_parts(
        DigestDomain::RequestPlan,
        EXPOSURE_MAP_DIGEST_VERSION,
        &parts,
    )
}

fn push_identity_parts(parts: &mut Vec<Vec<u8>>, identity: &RegisteredToolIdentity) {
    parts.push(b"registered-tool".to_vec());
    parts.push(identity.tool_id().as_str().as_bytes().to_vec());
    parts.push(identity.name().as_str().as_bytes().to_vec());
    parts.push(identity.semantic_version().as_bytes().to_vec());
    parts.push(b"definition-digest".to_vec());
    parts.push(identity.definition_digest().identity_bytes());
    parts.push(b"schema-digest".to_vec());
    parts.push(identity.schema_digest().identity_bytes());
    parts.push(b"argument-limits-digest".to_vec());
    parts.push(identity.argument_limits_digest().identity_bytes());
    parts.push(identity.executor_id().as_str().as_bytes().to_vec());
    parts.push(b"executor-binding-identity".to_vec());
    parts.push(
        identity
            .executor_binding_identity()
            .digest()
            .identity_bytes(),
    );
    parts.push(b"effect-metadata-digest".to_vec());
    parts.push(identity.effect_metadata_digest().identity_bytes());
    parts.push(b"nested-eligibility".to_vec());
    parts.push(identity.nested_eligibility().identity_bytes().to_vec());
}

fn digest_parts(
    domain: DigestDomain,
    version: u32,
    parts: &[Vec<u8>],
) -> Result<Digest, ToolRuntimeError> {
    let borrowed = parts.iter().map(Vec::as_slice).collect::<Vec<_>>();
    Digest::sha256(domain, version, &borrowed).map_err(|error| {
        ToolRuntimeError::from_sensitive_source(
            ToolRuntimeErrorKind::Registration,
            "tool exposure digest could not be computed",
            &error,
        )
    })
}

fn registration_error(message: &'static str) -> ToolRuntimeError {
    ToolRuntimeError::new(ToolRuntimeErrorKind::Registration, message)
}

fn grok_type_to_glob(file_type: &str) -> Result<Value, ToolRuntimeError> {
    let glob = match file_type {
        "rust" => "*.rs",
        "py" => "*.py",
        "js" => "*.js",
        "ts" => "*.{ts,tsx}",
        "go" => "*.go",
        "java" => "*.java",
        "md" => "*.md",
        "json" => "*.json",
        other => {
            return Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::InvalidArguments,
                format!("grep type `{other}` is unsupported; use glob instead"),
            ));
        }
    };
    Ok(Value::String(glob.to_owned()))
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]
mod tests {
    use std::str::FromStr;
    use std::sync::Arc;

    use plakous_model::{
        RunId, ToolConcurrency, ToolEffect, ToolInputContract, ToolOutputContract,
    };
    use serde_json::{Value, json};

    use super::*;
    use crate::{
        RegisteredToolId, ToolExecutorId, ToolOutcome, ToolRegistration, ToolRegistryBuilder,
    };

    #[test]
    fn unified_exec_yield_clamps_long_requests_and_zero() {
        for (requested, expected) in [(120_000, 120_000), (900_000, 600_000), (0, 250)] {
            let arguments = json!({"yield_time_ms": requested});
            assert_eq!(
                unified_exec_yield_time_ms(arguments.as_object().unwrap()),
                expected
            );
        }
    }

    #[test]
    fn missing_required_fields_names_only_absent_schema_keys() {
        let schema = PortableToolSchemaV2::new(json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": {
                "template": {"type": "string"},
                "prompt": {"type": "string"},
                "output_schema_json": {"type": "string"},
                "tier": {"type": "string"}
            },
            "required": ["template", "prompt", "output_schema_json", "tier"],
            "additionalProperties": false
        }))
        .unwrap();

        assert_eq!(
            missing_required_fields(
                &schema,
                r#"{"template":"review","tier":"read_only","secret":"not echoed"}"#,
            )
            .as_deref(),
            Some("prompt, output_schema_json")
        );
        assert_eq!(missing_required_fields(&schema, "not json"), None);
    }

    fn registry_with_output(
        output: ToolOutputContract,
    ) -> Result<FrozenToolRegistry, Box<dyn std::error::Error>> {
        let definition = ToolDefinition::new(
            ToolName::new("read")?,
            "1.0.0",
            "SENSITIVE_DESCRIPTION_SENTINEL",
            ToolInputContract::structured(
                json!({"$schema":"https://json-schema.org/draft/2020-12/schema", "type":"object", "properties":{"SENSITIVE_SCHEMA_SENTINEL":{"type":"string"}},"required":["SENSITIVE_SCHEMA_SENTINEL"],"additionalProperties":false}),
            )?,
            output,
            ToolConcurrency::Serial,
            ToolEffect::ReadOnly,
        )?;
        let tool = Arc::new(crate::test_support::FixedTool::new(
            ToolOutcome::success_text("ok")?,
        ));
        let mut builder = ToolRegistryBuilder::default();
        builder.register(ToolRegistration::new(
            RegisteredToolId::new("read-id")?,
            ToolExecutorId::new("read-executor")?,
            crate::ToolExecutorBindingIdentity::new(1, &[b"read-executor-binding"])?,
            definition,
            tool,
        ))?;
        Ok(builder.freeze())
    }

    fn registry() -> Result<FrozenToolRegistry, Box<dyn std::error::Error>> {
        registry_with_output(ToolOutputContract::portable_model_content())
    }

    fn projection_target() -> Result<ProviderModelProjectionIdentityV2, Box<dyn std::error::Error>>
    {
        let effective = serde_json::from_value(serde_json::to_value(Digest::sha256(
            DigestDomain::EffectiveProviderConfiguration,
            5,
            &[b"projection-test-effective-configuration"],
        )?)?)?;
        Ok(ProviderModelProjectionIdentityV2::new(
            IntegrationClass::Custom,
            "custom.protocol",
            "custom.route",
            "custom.profile",
            "custom-family",
            "custom-model",
            "custom-model",
            effective,
            1,
        )?)
    }

    #[test]
    fn codex_patch_lowering_rejects_a_structured_canonical_target_at_freeze()
    -> Result<(), Box<dyn std::error::Error>> {
        let registry = registry()?;
        let identity = registry
            .identity(&ToolName::new("read")?)
            .ok_or("test canonical tool missing")?
            .clone();
        let target = projection_target()?;
        let entry = ToolProjectionExposureV2::new(
            [(CanonicalProcessOperationV2::Poll, identity)],
            "apply_patch",
            "Patch raw text",
            PortableToolSchemaV2::new(json!({
                "$schema":"https://json-schema.org/draft/2020-12/schema",
                "type":"object",
                "properties":{"patch":{"type":"string"}},
                "required":["patch"],
                "additionalProperties":false
            }))?,
            SemanticJsonV1::from_value(json!({}))?,
            ToolProjectionLoweringV2::CodexApplyPatch,
            PortableToolSchemaV2::new(json!({
                "$schema":"https://json-schema.org/draft/2020-12/schema",
                "type":"object",
                "properties":{},
                "required":[],
                "additionalProperties":false
            }))?,
            ToolProjectionRaisingV2::Identity,
            target.clone(),
        )?;
        assert_eq!(
            ToolProjectionMapV2::new(registry, target, [entry])
                .err()
                .map(|error| error.kind()),
            Some(ToolRuntimeErrorKind::Registration)
        );
        Ok(())
    }

    fn text_projection(
        raising: ToolProjectionRaisingV2,
    ) -> Result<(ToolProjectionMapV2, ToolProjectionReceiptV2), Box<dyn std::error::Error>> {
        text_projection_with_output(raising, ToolOutputContract::portable_model_content())
    }

    fn text_projection_with_output(
        raising: ToolProjectionRaisingV2,
        output: ToolOutputContract,
    ) -> Result<(ToolProjectionMapV2, ToolProjectionReceiptV2), Box<dyn std::error::Error>> {
        let registry = registry_with_output(output)?;
        let identity = registry
            .identity(&ToolName::new("read")?)
            .ok_or("registered identity is missing")?
            .clone();
        let target = projection_target()?;
        let entry = ToolProjectionExposureV2::new(
            [(CanonicalProcessOperationV2::Poll, identity.clone())],
            "read_file",
            "Read one value.",
            PortableToolSchemaV2::new(json!({
                "$schema":"https://json-schema.org/draft/2020-12/schema",
                "type":"object",
                "properties":{"SENSITIVE_SCHEMA_SENTINEL":{"type":"string"}},
                "required":["SENSITIVE_SCHEMA_SENTINEL"],
                "additionalProperties":false
            }))?,
            SemanticJsonV1::from_value(json!({}))?,
            ToolProjectionLoweringV2::CanonicalPassthrough,
            PortableToolSchemaV2::new(json!({
                "$schema":"https://json-schema.org/draft/2020-12/schema",
                "type":"object",
                "properties":{"result":{"type":"string"}},
                "required":["result"],
                "additionalProperties":false
            }))?,
            raising,
            target.clone(),
        )?;
        let receipt = ToolProjectionReceiptV2 {
            projection_identity: entry.identity,
            exposure_map_identity: Digest::sha256(DigestDomain::RequestPlan, 1, &[b"placeholder"])?,
            canonical_identity: identity,
            canonical_operation: CanonicalProcessOperationV2::Poll,
            exposed_name: ToolName::new("read_file")?,
            lowering_identity: entry.lowering_identity,
            raising_identity: entry.raising_identity,
            target: target.clone(),
        };
        let map = ToolProjectionMapV2::new(registry, target, [entry])?;
        let receipt = ToolProjectionReceiptV2 {
            exposure_map_identity: map.digest(),
            ..receipt
        };
        Ok((map, receipt))
    }

    #[test]
    fn identity_raising_validates_and_text_raising_matches_its_schema()
    -> Result<(), Box<dyn std::error::Error>> {
        let (identity, receipt) = text_projection(ToolProjectionRaisingV2::Identity)?;
        assert!(
            identity
                .raise(&receipt, ToolOutcome::success_text("plain text")?)
                .is_err()
        );

        let (wrapped, receipt) = text_projection(ToolProjectionRaisingV2::TextResultObjectV1)?;
        let outcome = wrapped.raise(&receipt, ToolOutcome::success_text("plain text")?)?;
        let [ToolResultPart::Text(text)] = outcome.parts() else {
            return Err("raised result was not text".into());
        };
        assert_eq!(
            serde_json::from_str::<Value>(text.text())?,
            json!({
                "result": "plain text"
            })
        );

        let (raw, receipt) = text_projection(ToolProjectionRaisingV2::RawUtf8V1)?;
        let outcome = raw.raise(&receipt, ToolOutcome::success_text("plain text")?)?;
        let [ToolResultPart::Text(text)] = outcome.parts() else {
            return Err("raw utf8 result was not text".into());
        };
        assert_eq!(text.text(), "plain text");
        Ok(())
    }

    #[test]
    fn text_result_object_raises_every_result_its_canonical_contract_permits()
    -> Result<(), Box<dyn std::error::Error>> {
        let (wrapped, receipt) = text_projection(ToolProjectionRaisingV2::TextResultObjectV1)?;
        // The largest canonical texts the default 1 MiB outcome bound admits:
        // plain bytes, and quotes that the wrapper escapes once more.
        for body in ["x".repeat(1_048_000), "\"".repeat(524_000)] {
            let canonical = ToolOutcome::success_text(body.clone())?;
            let outcome = wrapped.raise(&receipt, canonical)?;
            let [ToolResultPart::Text(text)] = outcome.parts() else {
                return Err("raised result was not text".into());
            };
            assert!(text.text().len() > SemanticJsonV1::maximum_string_bytes());
            assert_eq!(
                serde_json::from_str::<Value>(text.text())?,
                json!({ "result": body })
            );
        }
        Ok(())
    }

    #[test]
    fn text_result_raising_refuses_a_schema_that_does_not_admit_every_text_result()
    -> Result<(), Box<dyn std::error::Error>> {
        let registry = registry_with_output(ToolOutputContract::portable_model_content())?;
        let identity = registry
            .identity(&ToolName::new("read")?)
            .ok_or("registered identity is missing")?
            .clone();
        let input = PortableToolSchemaV2::new(json!({
            "$schema":"https://json-schema.org/draft/2020-12/schema",
            "type":"object",
            "properties":{},
            "required":[],
            "additionalProperties":false
        }))?;
        for properties in [
            json!({"result": {"type": "string"}, "extra": {"type": "string"}}),
            json!({"result": {"type": "string", "enum": ["only"]}}),
            json!({"result": {"type": "integer"}}),
        ] {
            let output = PortableToolSchemaV2::new(json!({
                "$schema":"https://json-schema.org/draft/2020-12/schema",
                "type":"object",
                "properties": properties,
                "required":["result"],
                "additionalProperties":false
            }))?;
            assert!(
                ToolProjectionExposureV2::new(
                    [(CanonicalProcessOperationV2::Poll, identity.clone())],
                    "read_file",
                    "Read one value.",
                    input.clone(),
                    SemanticJsonV1::from_value(json!({}))?,
                    ToolProjectionLoweringV2::CanonicalPassthrough,
                    output,
                    ToolProjectionRaisingV2::TextResultObjectV1,
                    projection_target()?,
                )
                .is_err()
            );
        }
        Ok(())
    }

    #[test]
    fn model_content_identity_validates_the_canonical_declared_contract()
    -> Result<(), Box<dyn std::error::Error>> {
        let contract = ToolOutputContract::model_content(2, 256)?;
        let (projection, receipt) =
            text_projection_with_output(ToolProjectionRaisingV2::ModelContentIdentityV1, contract)?;
        let outcome = ToolOutcome::success(vec![
            ToolResultPart::Text(plakous_model::TextBlock::new("first", None)?),
            ToolResultPart::Text(plakous_model::TextBlock::new("second", None)?),
        ])?;
        assert_eq!(projection.raise(&receipt, outcome.clone())?, outcome);

        let oversized = ToolOutcome::success_text("x".repeat(257))?;
        assert!(projection.raise(&receipt, oversized).is_err());
        Ok(())
    }

    #[test]
    fn constructors_validate_names_and_debug_redacts_definition_content()
    -> Result<(), Box<dyn std::error::Error>> {
        let registry = registry()?;
        let identity = registry
            .identity(&ToolName::new("read")?)
            .ok_or("registered identity is missing")?
            .clone();
        let invalid = ToolExposure::new(identity.clone(), "contains a space");
        assert_eq!(
            invalid.err().map(|error| error.kind()),
            Some(ToolRuntimeErrorKind::Registration)
        );

        let map = ToolExposureMap::new(registry, [ToolExposure::new(identity, "read_file")?])?;
        assert_eq!(map.registry_digest().domain(), DigestDomain::ToolDefinition);
        assert_eq!(map.registry_digest().version(), 9);
        assert_eq!(map.digest().domain(), DigestDomain::RequestPlan);
        assert_eq!(map.digest().version(), 8);
        assert_eq!(
            map.registry_digest().to_string(),
            "3a1a7da6f5e0d58c5120c4cb8372517503293a40f29c781dd1dbbe435d9acb2b"
        );
        let debug = format!("{map:?}");
        assert!(!debug.contains("SENSITIVE_DESCRIPTION_SENTINEL"));
        assert!(!debug.contains("SENSITIVE_SCHEMA_SENTINEL"));
        assert!(!debug.contains("properties"));
        Ok(())
    }

    #[test]
    fn description_override_is_model_facing_and_digest_bound()
    -> Result<(), Box<dyn std::error::Error>> {
        let registry = registry()?;
        let identity = registry
            .identity(&ToolName::new("read")?)
            .ok_or("registered identity is missing")?
            .clone();
        let canonical_digest = registry
            .definition(&ToolName::new("read")?)
            .ok_or("registered definition is missing")?
            .digest();
        let plain = ToolExposureMap::new(
            registry.clone(),
            [ToolExposure::new(identity.clone(), "read_file")?],
        )?;
        let overridden = ToolExposureMap::new(
            registry.clone(),
            [ToolExposure::new(identity, "read_file")?
                .with_description_override(Some("provider-facing description".to_owned()))?],
        )?;

        assert_eq!(
            overridden.definitions()[0].description(),
            "provider-facing description"
        );
        assert_eq!(
            registry
                .definition(&ToolName::new("read")?)
                .ok_or("registered definition is missing")?
                .digest(),
            canonical_digest
        );
        assert_ne!(plain.digest(), overridden.digest());
        assert_eq!(
            overridden.entries()[0].description_override(),
            Some("provider-facing description")
        );
        Ok(())
    }

    #[test]
    fn codex_absolute_workdir_is_canonicalized_only_beneath_the_active_cwd()
    -> Result<(), Box<dyn std::error::Error>> {
        let workspace = tempfile::tempdir()?;
        std::fs::create_dir(workspace.path().join("nested"))?;
        let environment = ExecutionEnvironment::host(
            crate::ExecutionEnvironmentScope::Run(RunId::from_str(
                "00000000-0000-0000-0000-000000000091",
            )?),
            workspace.path(),
            ".",
            crate::ExecutionEnvironmentConfig::default(),
        )?;

        assert_eq!(
            canonical_process_workdir(&environment, environment.cwd())?,
            Path::new(".")
        );
        assert_eq!(
            canonical_process_workdir(&environment, &environment.cwd().join("nested"))?,
            Path::new("nested")
        );
        assert_eq!(
            canonical_process_workdir(&environment, Path::new("nested"))?,
            Path::new("nested")
        );
        assert!(canonical_process_workdir(&environment, Path::new("../outside")).is_err());
        assert!(canonical_process_workdir(&environment, Path::new("/tmp/outside")).is_err());
        Ok(())
    }
}
