//! Provider-model tool projections: lowering and raising identities, exposure
//! entries, receipts and the frozen projection map.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;

use plakous_model::{
    CompiledToolProjectionV2, Digest, DigestDomain, EffectiveProviderConfigurationId,
    IntegrationClass, ProjectedCustomToolFormatV2, ProjectedToolDefinitionV2,
    ProjectedToolPresentationV2, SemanticJsonV1, ToolEffect, ToolInputContract, ToolName,
    ToolProjectionTargetV2, ToolResultPart, Utf8TextContractV1,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::lowering::{lower_projection, placeholder_for};
use super::map::{registration_error, registry_identity_digest};
use super::raising::{
    admits_every_text_result, raise_identity, raise_process_result, raise_read_observation,
    raise_text_result_object, validate_model_content_identity,
};
use crate::{
    ExecutionEnvironment, FrozenToolRegistry, PortableToolSchemaV2, RegisteredToolIdentity,
    ToolInvocation, ToolInvocationRequest, ToolOutcome, ToolRuntimeError, ToolRuntimeErrorKind,
};

const PROJECTION_MAP_DIGEST_VERSION: u32 = 4;

/// Names only absent top-level keys from the already bounded, model-visible
/// schema. Values from the rejected call are never copied into the error.
pub(super) fn missing_required_fields(schema: &PortableToolSchemaV2, raw: &str) -> Option<String> {
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
    pub(super) canonical_operations: BTreeMap<CanonicalProcessOperationV2, RegisteredToolIdentity>,
    source_canonical_name: ToolName,
    executor_id: crate::ToolExecutorId,
    exposed_name: ToolName,
    description: String,
    presentation: ProjectedToolPresentationV2,
    pub(super) defaults: SemanticJsonV1,
    pub(super) lowering: ToolProjectionLoweringV2,
    pub(super) lowering_identity: Digest,
    pub(super) output_schema: PortableToolSchemaV2,
    raising: ToolProjectionRaisingV2,
    pub(super) raising_identity: Digest,
    target: ProviderModelProjectionIdentityV2,
    pub(super) identity: Digest,
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
    /// Whether local programmatic callers receive the raised JSON object.
    /// Direct provider calls retain their text result representation.
    #[must_use]
    pub const fn local_programmatic_returns_object(&self) -> bool {
        matches!(
            self.lowering,
            ToolProjectionLoweringV2::CodexExecCommand | ToolProjectionLoweringV2::CodexWriteStdin
        )
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
    pub(super) projection_identity: Digest,
    pub(super) exposure_map_identity: Digest,
    pub(super) canonical_identity: RegisteredToolIdentity,
    pub(super) canonical_operation: CanonicalProcessOperationV2,
    pub(super) exposed_name: ToolName,
    pub(super) lowering_identity: Digest,
    pub(super) raising_identity: Digest,
    pub(super) target: ProviderModelProjectionIdentityV2,
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

/// Immutable V2 catalog allowing several exact projected names for one canonical capability.
#[derive(Clone)]
pub struct ToolProjectionMap {
    registry: FrozenToolRegistry,
    target: ProviderModelProjectionIdentityV2,
    entries: Arc<[ToolProjectionExposureV2]>,
    by_exposed: BTreeMap<ToolName, usize>,
    covered_canonical_names: BTreeSet<ToolName>,
    registry_digest: Digest,
    digest: Digest,
}

impl ToolProjectionMap {
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

fn compiled_projection_error(error: &plakous_model::RequestError) -> ToolRuntimeError {
    ToolRuntimeError::from_sensitive_source(
        ToolRuntimeErrorKind::Registration,
        "model request projection view rejected the exact tools-owned map",
        error,
    )
}

impl fmt::Debug for ToolProjectionMap {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ToolProjectionMap")
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
