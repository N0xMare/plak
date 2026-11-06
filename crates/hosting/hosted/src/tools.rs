//! Product tool packs: how a compiled-in bundle of tools is registered and how
//! it reaches a model-visible catalog.
//!
//! A declaration is a [`ToolPack`]: it supplies executable registrations and
//! the menu those registrations present. Registration alone never selects a
//! tool for composition — the pack list does.

use std::sync::Arc;

#[cfg(feature = "web-fetch")]
mod fetch_url;
#[cfg(not(feature = "web-fetch"))]
#[path = "tools/fetch_url_unavailable.rs"]
mod fetch_url;

#[cfg(any(
    feature = "codex",
    feature = "grok",
    feature = "claude",
    feature = "openai",
    feature = "anthropic",
    feature = "xai-api",
    feature = "openrouter"
))]
mod image_gen;
#[cfg(not(any(
    feature = "codex",
    feature = "grok",
    feature = "claude",
    feature = "openai",
    feature = "anthropic",
    feature = "xai-api",
    feature = "openrouter"
)))]
#[path = "tools/image_gen_unavailable.rs"]
mod image_gen;

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
    /// Canonical user skill roots this node's read may reach outside the
    /// workspace.
    skill_roots: Arc<[std::path::PathBuf]>,
    /// The skills listing this node's instructions carry after `AGENTS.md`.
    skills_listing: Option<plakous::agent::RegisteredContextSource>,
    /// Whether a registered `Skill` is presented: only on Claude vocabulary.
    /// Elsewhere it stays registered, so canonical tool authority does not
    /// move with the vocabulary, and is never shown (spec 01 §3).
    skill_presented: bool,
    omit_tool_images: bool,
}

impl ProductToolExtensions {
    pub(crate) const fn tool_result_images(&self) -> bool {
        !self.omit_tool_images
    }
    pub(crate) fn admit_sdk(&self) -> CliResult<()> {
        let names = self.names().map(ToString::to_string).collect::<Vec<_>>();
        admit_sdk_extensions(names)
    }

    /// The current product ships no additional composition tools.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    pub(crate) fn process_config(&self) -> plakous::tools::ProcessPackConfig {
        plakous::tools::ProcessPackConfig {
            completion_notices: self.host_facts.host_wakes,
        }
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
            omit_tool_images: false,
            registrations: registrations.into(),
            host_facts: plakous_host_protocol::PromptHostFacts::default(),
            retrieval: None,
            image_gen: None,
            contributions: Arc::new([]),
            skill_roots: Arc::new([]),
            skills_listing: None,
            skill_presented: false,
        })
    }

    /// Adds one node's skills: the roots its read may reach, its listing,
    /// and while its skills are on the `Skill` registration, appended last
    /// and presented on Claude vocabulary only.
    /// Call it after [`Self::with_plan`], which rebuilds the extensions.
    pub(crate) fn with_skills(mut self, skills: &crate::skills::NodeSkills) -> CliResult<Self> {
        self.skill_roots = skills.read_roots().into();
        self.skills_listing = skills.listing_source()?;
        self.skill_presented = skills.presented();
        if let Some(registration) = skills.tool_registration()? {
            if self.names().any(|name| name == registration.name()) {
                return Err(CliFailure::new(
                    "product tool extensions contain a duplicate tool",
                ));
            }
            self.registrations = self
                .registrations
                .iter()
                .cloned()
                .chain([registration])
                .collect();
        }
        Ok(self)
    }

    /// Whether a tool other than `Skill` extends the catalog: what rotates a
    /// profile id to its `.subagents.v1` form, which `Skill` alone does not.
    pub(crate) fn extends_beyond_skills(&self) -> bool {
        self.names()
            .any(|name| name.as_str() != crate::skills::SKILL_TOOL_NAME)
    }

    pub(crate) fn skill_roots(&self) -> &[std::path::PathBuf] {
        &self.skill_roots
    }

    pub(crate) fn skills_listing(&self) -> Option<&plakous::agent::RegisteredContextSource> {
        self.skills_listing.as_ref()
    }

    #[cfg(test)]
    pub fn new(registrations: impl IntoIterator<Item = ToolRegistration>) -> CliResult<Self> {
        Self::from_nested_registrations(registrations)
    }

    #[must_use]
    /// Whether no member is presented.
    pub fn is_empty(&self) -> bool {
        self.names().next().is_none()
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn registrations(&self) -> &[ToolRegistration] {
        &self.registrations
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
    pub(crate) fn with_plan(mut self, plan: &crate::tool_plan::ToolPlan) -> CliResult<Self> {
        self.omit_tool_images = !plan.tool_result_images();
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
            .map_err(|error| CliFailure::new(format!("hosted retrieval wrappers: {error}")))?
            .with_options(plan.retrieval_options().clone());
            appended.extend(
                pack.registrations().map_err(|error| {
                    CliFailure::new(format!("hosted retrieval wrappers: {error}"))
                })?,
            );
        }
        if fetch_url {
            appended.extend(fetch_url::registrations(
                plan.fetch_url_limits(),
                plan.tool_result_images(),
            )?);
        }
        let mut image_slot = None;
        if image_gen {
            let binding = plan
                .image_gen()
                .ok_or_else(|| CliFailure::new("planned image_gen has no image port"))?;
            image_slot = binding.slot();
            appended.extend(image_gen::registrations(binding.port())?);
        }
        let earning = mcp.first().map(|registration| registration.name().clone());
        appended.extend(mcp);
        let host_facts = self.host_facts;
        let mut extended =
            Self::from_nested_registrations(self.registrations.iter().cloned().chain(appended))?
                .with_host_facts(host_facts);
        extended.retrieval = slot;
        extended.image_gen = image_slot;
        extended.omit_tool_images = self.omit_tool_images;
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

    /// The presented members, in order: every registration except a `Skill`
    /// this vocabulary does not present.
    pub fn names(&self) -> impl Iterator<Item = &ToolName> {
        self.registrations
            .iter()
            .map(ToolRegistration::name)
            .filter(|name| self.skill_presented || name.as_str() != crate::skills::SKILL_TOOL_NAME)
    }
}

pub(crate) fn admit_sdk_extensions(names: Vec<String>) -> CliResult<()> {
    if names.is_empty() {
        return Ok(());
    }
    Err(CliFailure::new(format!(
        "py-sdk does not support configured optional tools or MCP extensions: {}; remove them from the root template's tools key or select another [root] composition",
        names.join(", "),
    )))
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
    let mut packs = if runtime == Some(plakous::tools::CompositionRuntime::PythonHarnessSdk) {
        if fence != crate::agent::SessionToolFence::Full {
            return Err(CliFailure::new(
                "py-sdk requires a single-agent root with the full tool fence; child tiers are unavailable",
            ));
        }
        extensions.admit_sdk()?;
        let descriptor = plakous::tools::PythonHarnessSdkV1::compile(
            job_config,
            registry.clone(),
            target.clone(),
        )
        .map_err(|error| CliFailure::new(format!("Python SDK catalog is invalid: {error}")))?;
        let names = descriptor
            .nested_projection()
            .entries()
            .iter()
            .map(|entry| entry.exposed_name().clone());
        vec![Arc::new(PassthroughPack::new(names)) as Arc<dyn ToolPack>]
    } else {
        standard_coding_menus(job_config)
    };
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
    // A cell returns text. `view_image` stays on the Direct menu (runtime
    // `None`) and is refused at call time when the model has no image input.
    if runtime.is_some() {
        packs = packs
            .into_iter()
            .map(|pack| Arc::new(CellTextMenu { pack }) as Arc<dyn ToolPack>)
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

/// Drops `view_image` from a program surface. Image results are not a cell's
/// catalog, so the nested names and the prompt that lists them stay the Direct
/// menu without that tool.
struct CellTextMenu {
    pack: Arc<dyn ToolPack>,
}

impl ToolPack for CellTextMenu {
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
            .filter(|entry| entry.exposed() != "view_image")
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
mod tests;
