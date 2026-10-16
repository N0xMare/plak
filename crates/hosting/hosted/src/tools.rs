//! Product tool packs: how a compiled-in bundle of tools is registered and how
//! it reaches a model-visible catalog.
//!
//! A declaration is a [`ToolPack`]: it supplies executable registrations and
//! the menu those registrations present. Registration alone never selects a
//! tool for composition — the pack list does.

use std::sync::Arc;

use plakous::model::ToolName;
use plakous::tools::{
    CodingPresentation, CompiledSurface, Earned, FrozenToolRegistry, MenuEntry, PassthroughPack,
    PromptContribution, PromptPin, ProviderModelProjectionIdentityV2, ToolPack, ToolRegistration,
    ToolRegistryBuilder, ToolRuntimeError, compile_over, register_packs, standard_coding_menus,
};

use crate::error::{CliFailure, CliResult};
use plakous::prompts::identity::{
    ProductAgreementPack, UNTRUSTED_TOOL_DATA_AGREEMENTS, UNTRUSTED_TOOL_DATA_CONTRIBUTION_ID,
};

/// One immutable ordered declaration of product-owned tool extensions.
///
/// Every member is nested-eligible and is presented exactly as its own
/// `ToolDefinition` describes it: a product tool says how it looks by saying
/// what it is, and the host adds no text of its own.
#[derive(Clone, Default)]
pub struct ProductToolExtensions {
    registrations: Arc<[ToolRegistration]>,
    host_facts: plakous_host_protocol::PromptHostFacts,
    retrieval: Option<Arc<crate::tool_plan::DeferredRetrievalPort>>,
    image_gen: Option<Arc<crate::tool_plan::DeferredImagePort>>,
    contributions: Arc<[PromptContribution]>,
}

impl ProductToolExtensions {
    /// The current product ships no additional composition tools.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    pub(crate) fn with_host_facts(mut self, facts: plakous_host_protocol::PromptHostFacts) -> Self {
        self.host_facts = facts;
        self
    }

    /// Ordered nested-eligible product extensions, including topology tools.
    pub fn from_nested_registrations(
        registrations: impl IntoIterator<Item = ToolRegistration>,
    ) -> CliResult<Self> {
        let registrations = registrations.into_iter().collect::<Vec<_>>();
        let mut names = std::collections::BTreeSet::new();
        for registration in &registrations {
            if !registration.is_nested_eligible() {
                return Err(CliFailure::new(
                    "product tool extension is not explicitly nested-eligible",
                ));
            }
            if !names.insert(registration.name().clone()) {
                return Err(CliFailure::new(
                    "product tool extensions contain a duplicate tool",
                ));
            }
        }
        Ok(Self {
            registrations: registrations.into(),
            host_facts: plakous_host_protocol::PromptHostFacts::default(),
            retrieval: None,
            image_gen: None,
            contributions: Arc::new([]),
        })
    }

    #[cfg(test)]
    pub fn new(registrations: impl IntoIterator<Item = ToolRegistration>) -> CliResult<Self> {
        Self::from_nested_registrations(registrations)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.registrations.is_empty()
    }

    /// Appends a bound tool plan after these extensions: the one append path
    /// for every tool a template selects, in plan order.
    ///
    /// Hosted-retrieval wrappers register through their pack on the port the
    /// plan bound; `fetch_url` follows through its own pack under the plan's
    /// limits, then `image_gen` on the plan's image port; MCP tools append the registrations MCP binding froze, and
    /// the first one earns the untrusted-data paragraph. A planned tool
    /// without a registration refuses the bind. The empty plan leaves the
    /// extensions, and every surface and identity compiled from them,
    /// unchanged.
    pub(crate) fn with_plan(self, plan: &crate::tool_plan::ToolPlan) -> CliResult<Self> {
        let mut wrappers = Vec::new();
        let mut mcp = Vec::new();
        let mut fetch_url = false;
        let mut image_gen = false;
        for tool in plan.tools() {
            match tool {
                crate::tool_plan::PlannedTool::Optional(
                    crate::tool_plan::OptionalTool::FetchUrl,
                ) => fetch_url = true,
                crate::tool_plan::PlannedTool::Optional(
                    crate::tool_plan::OptionalTool::ImageGen,
                ) => image_gen = true,
                crate::tool_plan::PlannedTool::Optional(optional) => {
                    let wrapper = optional.web_tool().ok_or_else(|| {
                        CliFailure::new(format!(
                            "planned tool {tool:?} has no registration in this build"
                        ))
                    })?;
                    wrappers.push(wrapper);
                }
                crate::tool_plan::PlannedTool::Mcp { exposed, .. } => {
                    let registration = plan.mcp_registration(exposed).ok_or_else(|| {
                        CliFailure::new(format!("planned tool {exposed} has no registration"))
                    })?;
                    mcp.push(registration.clone());
                }
            }
        }
        if wrappers.is_empty() && mcp.is_empty() && !fetch_url && !image_gen {
            return Ok(self);
        }
        let mut appended = Vec::new();
        let mut slot = None;
        if !wrappers.is_empty() {
            let binding = plan
                .retrieval()
                .ok_or_else(|| CliFailure::new("planned hosted retrieval has no retrieval port"))?;
            slot = binding.slot();
            let pack = plakous::tools::web::WebToolPack::new(
                binding.presentation(),
                wrappers,
                binding.port(),
            )
            .map_err(|error| CliFailure::new(format!("hosted retrieval wrappers: {error}")))?;
            appended.extend(
                pack.registrations().map_err(|error| {
                    CliFailure::new(format!("hosted retrieval wrappers: {error}"))
                })?,
            );
        }
        if fetch_url {
            let pack = plakous::tools::web::FetchUrlPack::new(
                plan.fetch_url_limits(),
                &format!("plak/{}", plakous::VERSION),
            )
            .map_err(|error| CliFailure::new(format!("fetch_url: {error}")))?;
            appended.extend(
                pack.registrations()
                    .map_err(|error| CliFailure::new(format!("fetch_url: {error}")))?,
            );
        }
        let mut image_slot = None;
        if image_gen {
            let binding = plan
                .image_gen()
                .ok_or_else(|| CliFailure::new("planned image_gen has no image port"))?;
            image_slot = binding.slot();
            appended.extend(
                plakous::tools::web::ImageGenPack::new(binding.port())
                    .registrations()
                    .map_err(|error| CliFailure::new(format!("image_gen: {error}")))?,
            );
        }
        let earning = mcp.first().map(|registration| registration.name().clone());
        appended.extend(mcp);
        let host_facts = self.host_facts;
        let mut extended =
            Self::from_nested_registrations(self.registrations.iter().cloned().chain(appended))?
                .with_host_facts(host_facts);
        extended.retrieval = slot;
        extended.image_gen = image_slot;
        if let Some(earning) = earning {
            extended.contributions = vec![PromptContribution::new(
                UNTRUSTED_TOOL_DATA_CONTRIBUTION_ID,
                Earned::ToolPresented(earning),
                UNTRUSTED_TOOL_DATA_AGREEMENTS,
            )]
            .into();
        }
        Ok(extended)
    }

    /// Connects the session's retrieval executor to the wrappers bound
    /// before it existed. Without bound wrappers the port is dropped.
    pub(crate) fn connect_retrieval(
        &self,
        port: Option<Arc<dyn plakous::tools::web::RetrievalPort>>,
    ) -> CliResult<()> {
        let Some(slot) = &self.retrieval else {
            return Ok(());
        };
        let port = port.ok_or_else(|| {
            CliFailure::new(
                "hosted retrieval tools are bound but the route built no retrieval port",
            )
        })?;
        slot.connect(port).map_err(CliFailure::new)
    }

    /// Connects the session's image executor to the `image_gen` bound
    /// before it existed. Without a bound `image_gen` the port is dropped.
    pub(crate) fn connect_image_gen(
        &self,
        port: Option<Arc<dyn plakous::tools::web::ImageGenPort>>,
    ) -> CliResult<()> {
        let Some(slot) = &self.image_gen else {
            return Ok(());
        };
        let port = port.ok_or_else(|| {
            CliFailure::new("image_gen is bound but the route built no image port")
        })?;
        slot.connect(port).map_err(CliFailure::new)
    }

    /// Registers this pack's executors through the pack seam.
    pub fn register_into(&self, builder: &mut ToolRegistryBuilder) -> CliResult<()> {
        register_packs(&[Arc::new(self.clone()) as Arc<dyn ToolPack>], builder).map_err(|error| {
            CliFailure::new(format!(
                "product tool extension could not be registered: {error}"
            ))
        })
    }

    pub fn names(&self) -> impl Iterator<Item = &ToolName> {
        self.registrations.iter().map(ToolRegistration::name)
    }
}

impl ToolPack for ProductToolExtensions {
    fn id(&self) -> &'static str {
        "plak.product-extensions.v1"
    }

    fn registrations(&self) -> Result<Vec<ToolRegistration>, ToolRuntimeError> {
        Ok(self.registrations.to_vec())
    }

    /// Every member appears under its canonical name and its own description,
    /// under every vocabulary, appended after the presentation's core menu in
    /// declaration order. A product tool has no provider-familiar shape to put
    /// on, so passthrough *is* the whole menu — the extras are not a second
    /// list the compiler has to be handed separately.
    fn menu(&self, _presentation: CodingPresentation) -> Result<Vec<MenuEntry>, ToolRuntimeError> {
        PassthroughPack::appended_menu(&self.names().cloned().collect::<Vec<_>>())
    }

    /// The untrusted-data paragraph, when a planned MCP tool earns it.
    fn contributions(&self) -> &[PromptContribution] {
        &self.contributions
    }

    /// A product pack pins nothing of its own: every tool string it shows the
    /// model is that tool's `ToolDefinition` description, pinned by the route
    /// render, and the untrusted-data paragraph is pinned with the identity
    /// text.
    fn pins(&self) -> &'static [PromptPin] {
        &[]
    }
}

/// The callable surface of one node: the presentation's core menu, every
/// product extension after it, and the instruction fragments those names earn.
///
/// This is the one place the product asks which agreement paragraphs a node
/// has earned. The names judged are `exposed_names()` — core plus extensions,
/// the same membership the js-ptc / py-ptc nested catalog lists — so a PTC hub
/// earns its topology paragraph although its Direct request carries only the
/// harness tools. The PTC base instruction lists those same names.
#[cfg(test)]
pub(crate) fn product_surface(
    job_config: plakous::tools::JobConfig,
    presentation: CodingPresentation,
    registry: FrozenToolRegistry,
    target: ProviderModelProjectionIdentityV2,
    extensions: &ProductToolExtensions,
) -> CliResult<CompiledSurface> {
    product_surface_with_fence(
        job_config,
        presentation,
        registry,
        target,
        extensions,
        crate::agent::SessionToolFence::Full,
        None,
    )
}

pub(crate) fn product_surface_with_fence(
    job_config: plakous::tools::JobConfig,
    presentation: CodingPresentation,
    registry: FrozenToolRegistry,
    target: ProviderModelProjectionIdentityV2,
    extensions: &ProductToolExtensions,
    fence: crate::agent::SessionToolFence,
    runtime: Option<plakous::tools::CompositionRuntime>,
) -> CliResult<CompiledSurface> {
    let mut packs = standard_coding_menus(job_config);
    packs.push(Arc::new(extensions.clone()));
    packs.push(Arc::new(ProductAgreementPack::with_host_wake(
        extensions.host_facts.host_wakes,
    )));
    if fence != crate::agent::SessionToolFence::Full {
        packs = packs
            .into_iter()
            .map(|pack| {
                Arc::new(OccupancyMenu {
                    pack,
                    registry: registry.clone(),
                }) as Arc<dyn ToolPack>
            })
            .collect();
    }
    compile_over(&packs, presentation, registry, target, runtime).map_err(|error| {
        match error.namespace_clash() {
            Some(clash) => CliFailure::new(format!(
                "tool namespace could not be constructed: {clash}; no inference was dispatched"
            )),
            None => CliFailure::new(format!(
                "product tool surface could not be compiled: {error}"
            )),
        }
    })
}

struct OccupancyMenu {
    pack: Arc<dyn ToolPack>,
    registry: FrozenToolRegistry,
}

impl ToolPack for OccupancyMenu {
    fn id(&self) -> &'static str {
        self.pack.id()
    }

    fn verify(
        &self,
        presentation: CodingPresentation,
        registry: &FrozenToolRegistry,
    ) -> Result<(), ToolRuntimeError> {
        self.pack.verify(presentation, registry)
    }

    fn menu(&self, presentation: CodingPresentation) -> Result<Vec<MenuEntry>, ToolRuntimeError> {
        Ok(self
            .pack
            .menu(presentation)?
            .into_iter()
            .filter(|entry| {
                entry.bindings().iter().all(|binding| {
                    self.registry
                        .definition(binding.canonical())
                        .is_some_and(|definition| {
                            crate::agent::occupancy_read_only_allows(
                                binding.canonical().as_str(),
                                definition.effect(),
                            )
                        })
                })
            })
            .collect())
    }

    fn contributions(&self) -> &[PromptContribution] {
        self.pack.contributions()
    }
    fn pins(&self) -> &'static [PromptPin] {
        self.pack.pins()
    }
}

/// Fake nested-eligible extensions and the surfaces they compile to, for the
/// prompt and profile tests that need a callable set without a topology tree.
#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::Arc;

    use plakous::model::{
        ToolConcurrency, ToolDefinition, ToolEffect, ToolInputContract, ToolName,
        ToolOutputContract,
    };
    use plakous::tools::{
        CodingPresentation, CompiledSurface, RegisteredToolId, ToolContext, ToolExecutor,
        ToolExecutorBindingIdentity, ToolExecutorId, ToolFuture, ToolInvocation, ToolOutcome,
        ToolRegistration,
    };
    use serde_json::json;

    use super::{ProductToolExtensions, product_surface};
    use crate::agent::coding_tool_registry_with_extensions;
    use crate::agent::rendered_prompts::suite::synthetic_target;

    pub(crate) type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

    #[derive(Debug)]
    struct FixedTool;

    impl ToolExecutor for FixedTool {
        fn execute(
            self: Arc<Self>,
            _invocation: ToolInvocation,
            _context: ToolContext,
        ) -> ToolFuture {
            Box::pin(async { ToolOutcome::success_text("ok") })
        }
    }

    /// One fake registration; `nested` marks it nested-eligible.
    pub(crate) fn registration(name: &str, nested: bool) -> TestResult<ToolRegistration> {
        let definition = ToolDefinition::new(
            ToolName::new(name)?,
            "plak.product-test.v1",
            format!("Call {name}."),
            ToolInputContract::structured(json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object",
                "properties": {},
                "required": [],
                "additionalProperties": false
            }))?,
            ToolOutputContract::portable_model_content(),
            ToolConcurrency::Serial,
            ToolEffect::ReadOnly,
        )?;
        let registration = ToolRegistration::new(
            RegisteredToolId::new(format!("plak.test.{name}.v1"))?,
            ToolExecutorId::new(format!("plak.test.{name}.executor.v1"))?,
            ToolExecutorBindingIdentity::new(1, &[name.as_bytes()])?,
            definition,
            Arc::new(FixedTool),
        );
        Ok(if nested {
            registration.nested_eligible()
        } else {
            registration
        })
    }

    /// Nested-eligible fake extensions under the given names.
    pub(crate) fn extensions(names: &[&str]) -> ProductToolExtensions {
        ProductToolExtensions::new(
            names
                .iter()
                .map(|name| registration(name, true).expect("fake registration")),
        )
        .expect("fake extensions")
    }

    /// The compiled surface for one presentation over the coding registry plus
    /// the given fake extensions — the production `product_surface` path.
    pub(crate) fn surface(presentation: CodingPresentation, names: &[&str]) -> CompiledSurface {
        let extensions = extensions(names);
        let registry = coding_tool_registry_with_extensions(
            "test-surface",
            crate::binding::CompositionSelection::Direct,
            false,
            &extensions,
        )
        .expect("test registry");
        product_surface(
            plakous::tools::JobConfig::default(),
            presentation,
            registry,
            synthetic_target(b"test-surface"),
            &extensions,
        )
        .expect("test surface")
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{TestResult, extensions, registration, surface};
    use super::*;

    fn cli<T>(result: CliResult<T>) -> TestResult<T> {
        result.map_err(|error| std::io::Error::other(error.message()).into())
    }

    #[test]
    fn catalog_preserves_core_then_extension_declaration_order() -> TestResult {
        // One membership: the compiled surface. Core in the presentation's own
        // order, then every extension in declaration order.
        assert_eq!(
            surface(
                CodingPresentation::Codex,
                &["custom_second", "custom_first"]
            )
            .exposed_names()
            .iter()
            .map(ToolName::as_str)
            .collect::<Vec<_>>(),
            [
                "apply_patch",
                "exec_command",
                "update_plan",
                "write_stdin",
                "custom_second",
                "custom_first",
            ]
        );
        Ok(())
    }

    #[test]
    fn duplicate_extensions_core_collisions_and_direct_only_fail_closed() -> TestResult {
        assert!(
            ProductToolExtensions::new([
                registration("duplicate", true)?,
                registration("duplicate", true)?,
            ])
            .is_err()
        );
        assert!(ProductToolExtensions::new([registration("direct_only", false)?]).is_err());

        // An extension that collides with a canonical coding name fails closed
        // at the registry, before any menu is compiled.
        let collision = cli(ProductToolExtensions::new([registration(
            "apply_patch",
            true,
        )?]))?;
        assert!(
            crate::agent::coding_tool_registry_with_extensions(
                "collision",
                crate::binding::CompositionSelection::Direct,
                false,
                &collision,
            )
            .is_err()
        );

        // And a menu-level collision — an extension name a pack already
        // presents — fails closed in `compile_over`.
        let extensions = extensions(&["custom_only"]);
        let registry = crate::agent::coding_tool_registry_with_extensions(
            "menu-collision",
            crate::binding::CompositionSelection::Direct,
            false,
            &extensions,
        )
        .expect("registry");
        let doubled = cli(ProductToolExtensions::new([
            registration("custom_only", true)?,
            registration("custom_only", true)?,
        ]));
        assert!(doubled.is_err());
        assert!(
            product_surface(
                plakous::tools::JobConfig::default(),
                CodingPresentation::Codex,
                registry,
                crate::agent::rendered_prompts::suite::synthetic_target(b"menu-collision"),
                &extensions,
            )
            .is_ok()
        );
        Ok(())
    }

    #[test]
    fn an_empty_plan_leaves_extensions_registry_and_surface_unchanged() -> TestResult {
        use crate::tool_plan::{TemplateToolSelection, ToolPlan};

        let compile = |extensions: &ProductToolExtensions, presentation| {
            let registry = cli(crate::agent::coding_tool_registry_with_extensions(
                "empty-plan",
                crate::binding::CompositionSelection::Direct,
                false,
                extensions,
            ))?;
            cli(product_surface(
                plakous::tools::JobConfig::default(),
                presentation,
                registry,
                crate::agent::rendered_prompts::suite::synthetic_target(b"empty-plan"),
                extensions,
            ))
        };
        for presentation in [
            CodingPresentation::Codex,
            CodingPresentation::Claude,
            CodingPresentation::Grok,
            CodingPresentation::Pi,
        ] {
            for names in [&[][..], &["submit_result", "custom_tool"][..]] {
                let base = extensions(names);
                let planned = cli(base.clone().with_plan(&ToolPlan::default()))?;
                assert_eq!(
                    planned.names().collect::<Vec<_>>(),
                    base.names().collect::<Vec<_>>()
                );
                let (before, after) = (
                    compile(&base, presentation)?,
                    compile(&planned, presentation)?,
                );
                assert_eq!(after.exposed_names(), before.exposed_names());
                assert_eq!(
                    after.projection().registry_digest(),
                    before.projection().registry_digest()
                );
                assert_eq!(
                    after.registry().definitions(),
                    before.registry().definitions()
                );
            }
        }
        let selection = TemplateToolSelection::parse(["web_search"])?;
        assert!(
            extensions(&[])
                .with_plan(&ToolPlan::new(&selection, []))
                .is_err()
        );
        let unbound = ToolPlan::new(
            &TemplateToolSelection::parse(["mcp:fake"])?,
            [(
                plakous::tools::McpServerName::new("fake")?,
                ToolName::new("mcp__fake__echo")?,
            )],
        );
        assert!(extensions(&[]).with_plan(&unbound).is_err());
        Ok(())
    }

    #[test]
    fn one_declaration_registers_the_exact_selected_names() -> TestResult {
        let extensions = cli(ProductToolExtensions::new([registration(
            "custom_tool",
            true,
        )?]))?;
        let mut builder = ToolRegistryBuilder::default();
        cli(extensions.register_into(&mut builder))?;
        let registry = builder.freeze();
        let names = surface(CodingPresentation::Grok, &["custom_tool"])
            .exposed_names()
            .to_vec();
        let selected = names.last().ok_or("extension name is missing")?;
        assert_eq!(selected.as_str(), "custom_tool");
        assert!(registry.definition(selected).is_some());
        assert!(
            registry
                .identity(selected)
                .is_some_and(|identity| identity.nested_eligibility()
                    == extensions.registrations[0].nested_eligibility())
        );
        Ok(())
    }

    #[test]
    fn occupancy_catalog_omits_file_writes_but_keeps_process_plan_and_tree() -> TestResult {
        for presentation in [
            CodingPresentation::Codex,
            CodingPresentation::Claude,
            CodingPresentation::Grok,
            CodingPresentation::Pi,
        ] {
            let extensions = extensions(&["submit_result", "adopt_candidate"]);
            let registry = crate::agent::coding_tool_registry_with_extensions(
                "occupancy-catalog",
                crate::binding::CompositionSelection::Direct,
                false,
                &extensions,
            )
            .expect("registry");
            for fence in [
                crate::agent::SessionToolFence::OccupancyReadOnly,
                crate::agent::SessionToolFence::OccupancyExecute,
            ] {
                let surface = cli(product_surface_with_fence(
                    plakous::tools::JobConfig::default(),
                    presentation,
                    registry.clone(),
                    crate::agent::rendered_prompts::suite::synthetic_target(b"occupancy-catalog"),
                    &extensions,
                    fence,
                    None,
                ))?;
                let names = surface
                    .exposed_names()
                    .iter()
                    .map(ToolName::as_str)
                    .collect::<Vec<_>>();
                for name in [
                    "apply_patch",
                    "Edit",
                    "Write",
                    "write_file",
                    "write",
                    "edit",
                    "search_replace",
                ] {
                    assert!(!names.contains(&name), "{presentation:?}: {name}");
                }
                for name in ["submit_result", "adopt_candidate"] {
                    assert!(names.contains(&name));
                }
                // Claude (five-tool pack) and pi present no plan tool.
                assert_eq!(
                    names
                        .iter()
                        .any(|name| matches!(*name, "update_plan" | "todo_write" | "merge_plan")),
                    !matches!(
                        presentation,
                        CodingPresentation::Claude | CodingPresentation::Pi
                    ),
                    "{presentation:?}: {names:?}"
                );
                for name in ["update_plan", "merge_plan"] {
                    let name = ToolName::new(name)?;
                    assert_eq!(
                        registry
                            .definition(&name)
                            .ok_or("plan definition")?
                            .effect(),
                        plakous::model::ToolEffect::Mutating
                    );
                }
                assert!(names.iter().any(|name| matches!(
                    *name,
                    "exec_command" | "Bash" | "run_terminal_command" | "bash"
                )));
                for entry in surface.projection().entries() {
                    for identity in entry.canonical_operations().values() {
                        let definition =
                            registry.definition(identity.name()).ok_or("definition")?;
                        assert!(crate::agent::occupancy_read_only_allows(
                            definition.name().as_str(),
                            definition.effect()
                        ));
                    }
                }
            }
        }
        Ok(())
    }
}
