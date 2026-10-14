//! Compiling a coding vocabulary over the packs that fill it.
//!
//! `CodingPresentation` is vocabulary: which names a model is shown. Turning
//! one into a compiled menu needs the packs that own those names, and a pack
//! lives above the core, so the compile lives here with the pack list.
//!
//! The methods are an extension trait rather than free functions because the
//! question they answer really is "what does *this vocabulary* look like" —
//! the receiver is the presentation, and only the answer needs the packs.

use std::collections::BTreeSet;
use std::sync::Arc;

use plakous_model::{ProjectedToolPresentationV2, ToolName};
use plakous_tool_files::FilesToolPack;
use plakous_tool_plan::{InMemoryPlanState, PlanState, PlanToolPack};
use plakous_tool_process::{ProcessSessionTools, ProcessToolPack};
use plakous_tool_search::SearchToolPack;
use plakous_tools::{
    CodingPresentation, CompiledSurface, FrozenToolRegistry, NestedToolEligibility,
    PassthroughPack, PortableToolSchemaV2, ProviderModelProjectionIdentityV2, ToolPack,
    ToolProjectionExposureV2, ToolProjectionLoweringV2, ToolProjectionMapV2,
    ToolProjectionRaisingV2, ToolRuntimeError, ToolRuntimeErrorKind, compile_over,
    is_composition_marker_executor, object_schema,
};
use serde_json::json;

/// The four provider-neutral coding packs over one plan-state port and one
/// process service, in composition order.
///
/// Each pack owns its own registrations; nothing above them registers a coding
/// executor. The order is declaration order only — the menu ordinals decide the
/// model-visible order — so adding a pack cannot silently reorder a menu.
#[must_use]
pub fn standard_coding_packs(
    plan_state: Arc<dyn PlanState>,
    process: Arc<ProcessSessionTools>,
) -> Vec<Arc<dyn ToolPack>> {
    vec![
        Arc::new(FilesToolPack) as Arc<dyn ToolPack>,
        Arc::new(SearchToolPack),
        Arc::new(ProcessToolPack::new(
            process,
            plakous_tools::JobConfig::default(),
        )),
        Arc::new(PlanToolPack::new(plan_state)),
    ]
}

/// The four coding packs as menus only.
///
/// A menu-only pack presents capabilities an already frozen registry holds and
/// declares no registration of its own: it is what `compile_over` needs when
/// the session's registry already exists.
#[must_use]
pub fn standard_coding_menus(job_config: plakous_tools::JobConfig) -> Vec<Arc<dyn ToolPack>> {
    vec![
        Arc::new(FilesToolPack) as Arc<dyn ToolPack>,
        Arc::new(SearchToolPack),
        Arc::new(ProcessToolPack::menu_only(job_config)),
        Arc::new(PlanToolPack::menu_only()),
    ]
}

/// The four coding packs over process-local plan state and a POSIX shell
/// process service: the default composition for a session that supplies no
/// plan-state port of its own.
///
/// # Errors
///
/// Returns a definition error when a built-in invariant cannot be frozen.
pub fn standard_coding_packs_in_memory() -> Result<Vec<Arc<dyn ToolPack>>, ToolRuntimeError> {
    Ok(standard_coding_packs(
        Arc::new(InMemoryPlanState::new()),
        Arc::new(ProcessSessionTools::posix_sh()?),
    ))
}

/// Compiling one coding vocabulary over the standard coding packs.
///
/// Every compilation receives the session's bound process budgets, including
/// when reconstructing a persisted Direct or nested presentation.
pub trait CodingPresentationExt {
    /// Compiles the exact request-visible catalog over one frozen registry.
    ///
    /// # Errors
    ///
    /// Rejects a registry that is missing a catalog capability or an invalid projection.
    fn projection_map(
        self,
        job_config: plakous_tools::JobConfig,
        registry: FrozenToolRegistry,
        target: ProviderModelProjectionIdentityV2,
    ) -> Result<ToolProjectionMapV2, ToolRuntimeError>;

    /// Compiles the enrolled OAuth menu plus an explicit product extension list.
    ///
    /// Empty extras preserve the ordinary Direct catalog. `NestedEligible` or
    /// registry membership alone never selects an extension.
    ///
    /// # Errors
    ///
    /// Rejects a registry that is missing a catalog capability, colliding
    /// extras, or an invalid projection.
    fn projection_map_with_extensions(
        self,
        job_config: plakous_tools::JobConfig,
        registry: FrozenToolRegistry,
        target: ProviderModelProjectionIdentityV2,
        extra_nested: &[ToolName],
    ) -> Result<ToolProjectionMapV2, ToolRuntimeError>;

    /// The compiled surface for this vocabulary: the four coding packs plus the
    /// named extensions as one appended passthrough pack.
    ///
    /// This is the one membership computation. `CompiledSurface::exposed_names`
    /// is the nested-catalog list, the Direct catalog order, and the set the
    /// earned instruction fragments are judged against.
    ///
    /// # Errors
    ///
    /// Rejects a registry that is missing a catalog capability, colliding
    /// extras, an extension that is not explicitly nested-eligible, and every
    /// invalid projection.
    fn compile_surface(
        self,
        job_config: plakous_tools::JobConfig,
        registry: FrozenToolRegistry,
        target: ProviderModelProjectionIdentityV2,
        extra_nested: &[ToolName],
    ) -> Result<CompiledSurface, ToolRuntimeError>;

    /// Compiles Direct extras while omitting named exposed tools.
    ///
    /// Used when a wire family cannot lower a catalog member (Chat cannot
    /// lower `inspect_image` image tool results).
    ///
    /// # Errors
    ///
    /// Same fail-closed cases as [`Self::projection_map_with_extensions`].
    fn projection_map_with_extensions_omitting(
        self,
        job_config: plakous_tools::JobConfig,
        registry: FrozenToolRegistry,
        target: ProviderModelProjectionIdentityV2,
        extra_nested: &[ToolName],
        omit_exposed: &[&str],
    ) -> Result<ToolProjectionMapV2, ToolRuntimeError>;

    /// The extension list a Direct `projection` of this vocabulary was
    /// compiled with: every entry that is not the core entry of its name.
    ///
    /// A claimed slot is an extension: its entry binds the capability
    /// registered under exactly that exposed name, not the pack's.
    ///
    /// # Errors
    ///
    /// Rejects a registry the core catalog cannot be compiled over.
    fn projection_extensions(
        self,
        job_config: plakous_tools::JobConfig,
        registry: FrozenToolRegistry,
        projection: &ToolProjectionMapV2,
    ) -> Result<Vec<ToolName>, ToolRuntimeError>;

    /// Nested projection whose exposed names, schemas, and lowerings match Direct.
    ///
    /// `ordered_exposed_names` is the exact nested catalog (core Direct names
    /// plus extensions). Members that are not Direct core names are compiled as
    /// nested-eligible passthrough extras.
    ///
    /// # Errors
    ///
    /// Rejects missing Direct members, duplicate exposed names, composition
    /// markers, and Direct-only extras.
    fn compile_nested_projection(
        self,
        job_config: plakous_tools::JobConfig,
        registry: FrozenToolRegistry,
        target: ProviderModelProjectionIdentityV2,
        ordered_exposed_names: &[ToolName],
    ) -> Result<ToolProjectionMapV2, ToolRuntimeError>;
}

impl CodingPresentationExt for CodingPresentation {
    fn projection_map(
        self,
        job_config: plakous_tools::JobConfig,
        registry: FrozenToolRegistry,
        target: ProviderModelProjectionIdentityV2,
    ) -> Result<ToolProjectionMapV2, ToolRuntimeError> {
        self.projection_map_with_extensions(job_config, registry, target, &[])
    }

    fn projection_map_with_extensions(
        self,
        job_config: plakous_tools::JobConfig,
        registry: FrozenToolRegistry,
        target: ProviderModelProjectionIdentityV2,
        extra_nested: &[ToolName],
    ) -> Result<ToolProjectionMapV2, ToolRuntimeError> {
        Ok(self
            .compile_surface(job_config, registry, target, extra_nested)?
            .into_projection())
    }

    fn compile_surface(
        self,
        job_config: plakous_tools::JobConfig,
        registry: FrozenToolRegistry,
        target: ProviderModelProjectionIdentityV2,
        extra_nested: &[ToolName],
    ) -> Result<CompiledSurface, ToolRuntimeError> {
        let mut packs = standard_coding_menus(job_config);
        if !extra_nested.is_empty() {
            packs.push(Arc::new(PassthroughPack::new(extra_nested.iter().cloned())));
        }
        compile_over(&packs, self, registry, target, None)
    }

    fn projection_map_with_extensions_omitting(
        self,
        job_config: plakous_tools::JobConfig,
        registry: FrozenToolRegistry,
        target: ProviderModelProjectionIdentityV2,
        extra_nested: &[ToolName],
        omit_exposed: &[&str],
    ) -> Result<ToolProjectionMapV2, ToolRuntimeError> {
        let map = self.projection_map_with_extensions(
            job_config,
            registry.clone(),
            target.clone(),
            extra_nested,
        )?;
        if omit_exposed.is_empty() {
            return Ok(map);
        }
        let entries = map
            .entries()
            .iter()
            .filter(|entry| !omit_exposed.contains(&entry.exposed_name().as_str()))
            .cloned()
            .collect::<Vec<_>>();
        plakous_tools::internal::projection_map(registry, target, entries)
    }

    fn projection_extensions(
        self,
        job_config: plakous_tools::JobConfig,
        registry: FrozenToolRegistry,
        projection: &ToolProjectionMapV2,
    ) -> Result<Vec<ToolName>, ToolRuntimeError> {
        let core = self.projection_map(job_config, registry, projection.target().clone())?;
        Ok(extension_names(projection, &core))
    }

    fn compile_nested_projection(
        self,
        job_config: plakous_tools::JobConfig,
        registry: FrozenToolRegistry,
        target: ProviderModelProjectionIdentityV2,
        ordered_exposed_names: &[ToolName],
    ) -> Result<ToolProjectionMapV2, ToolRuntimeError> {
        let core = self
            .compile_surface(job_config, registry.clone(), target.clone(), &[])?
            .exposed_names()
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        // A claimable core name is an extension only when the registry identity
        // was flagged to claim it. An unflagged capability of that name is a
        // namespace clash, not a silent replacement of the core entry.
        let mut claimable = BTreeSet::new();
        for pack in standard_coding_menus(job_config) {
            for entry in pack.menu(self)? {
                if entry.is_claimable() {
                    claimable.insert(entry.exposed().to_owned());
                }
            }
        }
        let mut extras = Vec::new();
        for name in ordered_exposed_names {
            let claims = claimable.contains(name.as_str());
            let identity = registry.identity(name);
            if claims
                && identity
                    .is_some_and(plakous_tools::RegisteredToolIdentity::claims_presented_name)
            {
                extras.push(name.clone());
                continue;
            }
            if claims && identity.is_some() && core.contains(name) {
                return Err(plakous_tools::NamespaceClash::same_slot(
                    name.clone(),
                    plakous_tools::PresentationProvider::Pack("plakous.process.v3"),
                    plakous_tools::PresentationProvider::Pack("plakous.passthrough.v1"),
                )
                .into());
            }
            if !core.contains(name) {
                extras.push(name.clone());
            }
        }
        let direct = self.projection_map_with_extensions(
            job_config,
            registry.clone(),
            target.clone(),
            &extras,
        )?;
        let mut seen = BTreeSet::new();
        let mut selected = Vec::with_capacity(ordered_exposed_names.len());
        for name in ordered_exposed_names {
            if !seen.insert(name.clone()) {
                return Err(ToolRuntimeError::new(
                    ToolRuntimeErrorKind::Registration,
                    "product nested catalog contains a duplicate exposed name",
                ));
            }
            let entry = direct.by_exposed(name).cloned().ok_or_else(|| {
                ToolRuntimeError::new(
                    ToolRuntimeErrorKind::Registration,
                    "product nested catalog member is absent from the Direct projection",
                )
            })?;
            for identity in entry.canonical_operations().values() {
                if identity.nested_eligibility() != NestedToolEligibility::NestedEligible {
                    return Err(ToolRuntimeError::new(
                        ToolRuntimeErrorKind::Registration,
                        "product nested catalog member is not explicitly nested-eligible",
                    ));
                }
                if is_composition_marker_executor(identity.executor_id()) {
                    return Err(ToolRuntimeError::new(
                        ToolRuntimeErrorKind::Registration,
                        "composition outer markers cannot enter a nested catalog",
                    ));
                }
            }
            selected.push(nested_cell_raising(entry)?);
        }
        plakous_tools::internal::projection_map(registry, target, selected)
    }
}

/// The names of `projection`'s entries that are not `core`'s entry of the
/// same name, `core` being the same vocabulary compiled without extensions.
pub(crate) fn extension_names(
    projection: &ToolProjectionMapV2,
    core: &ToolProjectionMapV2,
) -> Vec<ToolName> {
    projection
        .entries()
        .iter()
        .filter(|entry| {
            core.by_exposed(entry.exposed_name())
                .is_none_or(|core| core.canonical_operations() != entry.canonical_operations())
        })
        .map(|entry| entry.exposed_name().clone())
        .collect()
}

fn nested_cell_raising(
    entry: ToolProjectionExposureV2,
) -> Result<ToolProjectionExposureV2, ToolRuntimeError> {
    if entry.raising() != ToolProjectionRaisingV2::TextResultObjectV1 {
        return Ok(entry);
    }
    if let ProjectedToolPresentationV2::CustomUtf8 { input, format } = entry.presentation() {
        return plakous_tools::internal::projection_exposure_custom(
            entry
                .canonical_operations()
                .iter()
                .map(|(operation, identity)| (*operation, identity.clone())),
            entry.exposed_name().as_str(),
            entry.description(),
            *input,
            format.clone(),
            entry.defaults().clone(),
            entry.lowering(),
            entry.output_schema().clone(),
            ToolProjectionRaisingV2::RawUtf8V1,
            entry.target().clone(),
        );
    }
    let description = if entry.lowering() == ToolProjectionLoweringV2::ClaudeReadFile {
        "Read a UTF-8 workspace file using file_path, offset and limit. Returns a JSON string with content (exact file text including line endings), offset, lines, eof, next_offset (null at EOF), and optional workspace_generation. Use JSON.parse(await tools.Read({...})).content in JavaScript or json.loads(await Read({...}))[\"content\"] in Python. Metadata is not file content; reject observation_truncated=true and check eof before treating the result as the entire file."
    } else {
        entry.description()
    };
    let input_schema = entry.input_schema().cloned().ok_or_else(|| {
        ToolRuntimeError::new(
            ToolRuntimeErrorKind::Registration,
            "nested text projection is not structured",
        )
    })?;
    let output_schema =
        PortableToolSchemaV2::new(object_schema(json!({}), &[])).map_err(|error| {
            ToolRuntimeError::from_sensitive_source(
                ToolRuntimeErrorKind::InvalidDefinition,
                "nested raw-utf8 output schema is invalid",
                &error,
            )
        })?;
    plakous_tools::internal::projection_exposure(
        entry
            .canonical_operations()
            .iter()
            .map(|(operation, identity)| (*operation, identity.clone())),
        entry.exposed_name().as_str(),
        description,
        input_schema,
        entry.defaults().clone(),
        entry.lowering(),
        output_schema,
        ToolProjectionRaisingV2::RawUtf8V1,
        entry.target().clone(),
    )
}
