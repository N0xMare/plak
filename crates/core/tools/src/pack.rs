//! The tool-pack seam: how a bundle of tools declares its registrations, how it
//! looks to a model under each coding vocabulary, and what instruction text it
//! earns.
//!
//! A pack owns four things that change together and are pinned together: the
//! executors it registers, the model-visible menu it presents per
//! [`CodingPresentation`], the instruction fragments its tools earn, and the
//! digests of every string it owns. [`compile`] turns an ordered pack list into
//! one projection over one frozen registry; nothing else may add a
//! model-visible name.

use std::collections::BTreeSet;
use std::sync::Arc;

use plakous_model::{
    ProjectedCustomToolFormatV2, SemanticJsonV1, ToolEffect, ToolInputContract, ToolName,
    Utf8TextContractV1,
};
use serde_json::{Value, json};

use crate::coding_presentation::CodingPresentation;
use crate::prompt_digest::PromptPin;
use crate::schema::object_schema;
use crate::{
    CanonicalProcessOperationV2, CompositionRuntime, FrozenToolRegistry, NamespaceClash,
    NamespacePresentation, NestedToolEligibility, PortableToolSchemaV2, PresentationProvider,
    ProviderModelProjectionIdentityV2, ToolProjectionExposureV2, ToolProjectionLoweringV2,
    ToolProjectionMapV2, ToolProjectionRaisingV2, ToolRegistration, ToolRegistryBuilder,
    ToolRuntimeError, ToolRuntimeErrorKind, resolve_namespace,
};

/// Which canonical operation an exposed name binds for one capability.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MenuOperation {
    /// The operation follows the capability's effect class: a read-only tool
    /// binds `Poll`, anything that may mutate binds `Start`.
    ByEffect,
    /// One explicitly pinned canonical operation.
    Exact(CanonicalProcessOperationV2),
}

/// One canonical capability an exposed menu entry dispatches to.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MenuBinding {
    operation: MenuOperation,
    canonical: ToolName,
}

impl MenuBinding {
    /// Binds one canonical capability with the operation its effect implies.
    #[must_use]
    pub const fn by_effect(canonical: ToolName) -> Self {
        Self {
            operation: MenuOperation::ByEffect,
            canonical,
        }
    }

    /// Binds one canonical capability to one exact operation.
    #[must_use]
    pub const fn exact(operation: CanonicalProcessOperationV2, canonical: ToolName) -> Self {
        Self {
            operation: MenuOperation::Exact(operation),
            canonical,
        }
    }

    /// Returns the canonical capability this binding dispatches to.
    #[must_use]
    pub const fn canonical(&self) -> &ToolName {
        &self.canonical
    }
}

/// How an exposed name is described and shaped for the model.
#[derive(Clone, Debug)]
pub enum MenuShape {
    /// Exposed under the canonical definition's own description and schema.
    ///
    /// This is the default for a pack with no provider-specific vocabulary, and
    /// the guarantee an extension relies on: its name and description reach the
    /// model exactly as its `ToolDefinition` states them.
    Canonical,
    /// A structured function with this pack's own description and schema.
    Structured {
        /// The model-visible description.
        description: Arc<str>,
        /// The model-visible JSON Schema.
        schema: Value,
    },
    /// A freeform custom-format tool: raw bounded UTF-8 under a grammar.
    CustomUtf8 {
        /// The model-visible description.
        description: Arc<str>,
        /// The bounded UTF-8 input contract.
        input: Utf8TextContractV1,
        /// The custom format the provider parses.
        format: ProjectedCustomToolFormatV2,
    },
}

/// The result envelope an exposed name raises into.
#[derive(Clone, Debug)]
pub enum MenuOutput {
    /// The `{ "result": string }` envelope every canonical Direct capability uses.
    ResultObject,
    /// An explicit output schema owned by the pack.
    Explicit(PortableToolSchemaV2),
}

/// Where one entry sits in a presentation's menu.
///
/// The menu is not a closed table in the core any more: every entry carries the
/// ordinal its pack declares, and [`compile`] orders the compiled menu by
/// `(ordinal, exposed name)`. [`MenuOrdinal::CORE`] is the presentation's own
/// menu, whose members are ordered by name; [`MenuOrdinal::appended`] places an
/// entry after the core menu at an exact position, so a pack that appends its
/// tools keeps its declaration order. A pack added to the composition therefore
/// contributes a first-class name without any edit to this crate.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct MenuOrdinal(u16);

impl MenuOrdinal {
    /// The presentation's core menu, ordered by exposed name.
    pub const CORE: Self = Self(0);

    /// One position after the core menu, in the declaring pack's own order.
    #[must_use]
    pub const fn appended(position: u16) -> Self {
        Self(position.saturating_add(1))
    }

    /// Reports whether this entry belongs to the presentation's core menu.
    #[must_use]
    pub const fn is_core(self) -> bool {
        self.0 == Self::CORE.0
    }
}

/// One model-visible entry a pack presents under one coding vocabulary.
#[derive(Clone, Debug)]
pub struct MenuEntry {
    exposed: Arc<str>,
    ordinal: MenuOrdinal,
    bindings: Vec<MenuBinding>,
    shape: MenuShape,
    defaults: Value,
    lowering: ToolProjectionLoweringV2,
    output: MenuOutput,
    raising: ToolProjectionRaisingV2,
    alternate: Option<Arc<str>>,
    claimable: bool,
    /// Bound job-lifetime tail copied from a core entry this one claimed.
    /// Appended to a canonical definition's description at resolution, so a
    /// claimed read/await/stop tool states the same foreground budget and
    /// hard runtime cap the leaf's description does.
    description_suffix: Option<Arc<str>>,
    /// Bound job-lifetime tail this entry's own description states, declared
    /// by the pack that rendered it. A claimer of this entry inherits it as
    /// its description suffix.
    lifetime_tail: Option<Arc<str>>,
}

impl MenuEntry {
    /// Declares one exposed name over one or more canonical bindings.
    #[must_use]
    pub fn new(
        exposed: &str,
        bindings: Vec<MenuBinding>,
        shape: MenuShape,
        lowering: ToolProjectionLoweringV2,
    ) -> Self {
        Self {
            exposed: Arc::from(exposed),
            ordinal: MenuOrdinal::CORE,
            bindings,
            shape,
            defaults: json!({}),
            lowering,
            output: MenuOutput::ResultObject,
            raising: ToolProjectionRaisingV2::TextResultObjectV1,
            alternate: None,
            claimable: false,
            description_suffix: None,
            lifetime_tail: None,
        }
    }

    /// Declares that a capability registered under exactly this exposed name
    /// and appended to the same surface replaces this core entry.
    ///
    /// The owning pack opts in per entry: a slot it presents that another
    /// layer may cover with one capability that also does this entry's work
    /// (Grok's output and kill tools on a node granted agent tools). Nothing
    /// else yields; an undeclared clash still fails construction.
    #[must_use]
    pub const fn claimable(mut self) -> Self {
        self.claimable = true;
        self
    }

    /// Reports whether [`Self::claimable`] was declared.
    #[must_use]
    pub const fn is_claimable(&self) -> bool {
        self.claimable
    }

    /// Appends `suffix` to this entry's model-visible description.
    ///
    /// Used when a claim takes over a core entry whose description carried the
    /// bound job-lifetime tail. The canonical definition text stays put; only
    /// the compiled surface grows the tail.
    #[must_use]
    pub fn with_description_suffix(mut self, suffix: &str) -> Self {
        self.description_suffix = Some(Arc::from(suffix));
        self
    }

    /// Declares the bound job-lifetime tail (foreground budget, hard runtime
    /// cap and job-lifetime contract) this entry's description states, so a
    /// claim that takes this entry over states the same bounds.
    #[must_use]
    pub fn with_lifetime_tail(mut self, tail: &str) -> Self {
        self.lifetime_tail = Some(Arc::from(tail));
        self
    }

    /// Declares the spelling this entry takes when a presentation of another
    /// slot holds its name in the same namespace.
    #[must_use]
    pub fn with_alternate(mut self, alternate: &str) -> Self {
        self.alternate = Some(Arc::from(alternate));
        self
    }

    /// Places this entry at an exact ordinal in the compiled menu.
    #[must_use]
    pub const fn with_ordinal(mut self, ordinal: MenuOrdinal) -> Self {
        self.ordinal = ordinal;
        self
    }

    /// Sets the model-visible argument defaults.
    #[must_use]
    pub fn with_defaults(mut self, defaults: Value) -> Self {
        self.defaults = defaults;
        self
    }

    /// Sets the output envelope this entry raises into.
    #[must_use]
    pub fn with_output(mut self, output: MenuOutput) -> Self {
        self.output = output;
        self
    }

    /// Sets the raising rule applied to canonical outcomes.
    #[must_use]
    pub const fn with_raising(mut self, raising: ToolProjectionRaisingV2) -> Self {
        self.raising = raising;
        self
    }

    /// Returns the model-visible name.
    #[must_use]
    pub fn exposed(&self) -> &str {
        &self.exposed
    }

    /// Returns the canonical bindings this entry dispatches to.
    #[must_use]
    pub fn bindings(&self) -> &[MenuBinding] {
        &self.bindings
    }

    /// Returns the ordinal this entry declared.
    #[must_use]
    pub const fn ordinal(&self) -> MenuOrdinal {
        self.ordinal
    }

    /// Returns the declared alternate spelling, if any.
    #[must_use]
    pub fn alternate(&self) -> Option<&str> {
        self.alternate.as_deref()
    }

    /// This entry as a namespace presentation from `provider`: its slots are
    /// its canonical bindings.
    fn presentation(
        &self,
        provider: PresentationProvider,
    ) -> Result<NamespacePresentation, ToolRuntimeError> {
        let presentation = NamespacePresentation::new(
            provider,
            tool_name(self.exposed(), "menu entry name is invalid")?,
            self.bindings
                .iter()
                .map(|binding| binding.canonical.clone()),
        );
        Ok(match self.alternate() {
            Some(alternate) => presentation.with_alternate(tool_name(
                alternate,
                "menu entry alternate spelling is invalid",
            )?),
            None => presentation,
        })
    }
}

/// What earns one instruction fragment.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Earned {
    /// Always present once the pack is composed.
    Always,
    /// Present when one named tool is on the compiled surface.
    ToolPresented(ToolName),
    /// Present when every named tool is on the compiled surface.
    AllPresented(Vec<ToolName>),
    /// Present when all required tools are callable and none of the excluded tools are.
    PresentedWithout {
        /// Tools that must be callable.
        required: Vec<ToolName>,
        /// Tools that must not be callable.
        excluded: Vec<ToolName>,
    },
}

impl Earned {
    /// Reports whether a compiled surface earns this fragment.
    #[must_use]
    pub fn is_earned_by(&self, presented: &BTreeSet<ToolName>) -> bool {
        match self {
            Self::Always => true,
            Self::ToolPresented(name) => presented.contains(name),
            Self::AllPresented(names) => names.iter().all(|name| presented.contains(name)),
            Self::PresentedWithout { required, excluded } => {
                required.iter().all(|name| presented.contains(name))
                    && excluded.iter().all(|name| !presented.contains(name))
            }
        }
    }
}

/// One instruction fragment a pack contributes when its tools earn it.
///
/// This replaces reading literal tool names out of a compiled surface at the
/// assembly site: the pack that owns the tool owns the paragraph that explains
/// it, and states the condition itself.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PromptContribution {
    id: &'static str,
    earned: Earned,
    text: Arc<str>,
}

impl PromptContribution {
    /// Declares one contribution.
    #[must_use]
    pub fn new(id: &'static str, earned: Earned, text: &str) -> Self {
        Self {
            id,
            earned,
            text: Arc::from(text),
        }
    }

    /// Returns the stable fragment id used by the pin table.
    #[must_use]
    pub const fn id(&self) -> &'static str {
        self.id
    }

    /// Returns the condition that earns this fragment.
    #[must_use]
    pub const fn earned(&self) -> &Earned {
        &self.earned
    }

    /// Returns the model-visible text.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }
}

/// One bundle of tools: its registrations, its per-presentation menu, the
/// instruction text its tools earn, and the digests of every string it owns.
pub trait ToolPack: Send + Sync {
    /// A stable pack id, e.g. `plakous.files.v3`.
    fn id(&self) -> &'static str;

    /// The executable registrations this pack contributes, in declaration order.
    ///
    /// The default is an empty list: a pack that only presents an already
    /// registered capability — as the four coding packs still do while
    /// `StandardCodingToolPack` owns registration order — declares no
    /// registration of its own.
    ///
    /// # Errors
    ///
    /// Returns the pack's own definition error when an invariant cannot freeze.
    fn registrations(&self) -> Result<Vec<ToolRegistration>, ToolRuntimeError> {
        Ok(Vec::new())
    }

    /// The model-visible entries this pack presents under one vocabulary.
    ///
    /// An empty menu is normal: a pack with nothing to say under Claude simply
    /// does not appear there.
    ///
    /// # Errors
    ///
    /// Returns an invariant error when a declared entry is not valid.
    fn menu(&self, presentation: CodingPresentation) -> Result<Vec<MenuEntry>, ToolRuntimeError>;

    /// Fails closed when the frozen registry does not carry this pack's exact
    /// durable identities.
    ///
    /// # Errors
    ///
    /// Returns a registration error when an identity is absent or has drifted.
    fn verify(
        &self,
        presentation: CodingPresentation,
        registry: &FrozenToolRegistry,
    ) -> Result<(), ToolRuntimeError> {
        let _ = (presentation, registry);
        Ok(())
    }

    /// Instruction fragments this pack's tools earn.
    fn contributions(&self) -> &[PromptContribution] {
        &[]
    }

    /// Every model-visible string this pack owns, with its pinned digest.
    fn pins(&self) -> &'static [PromptPin];
}

/// A pack that presents already-registered capabilities under their own names
/// and descriptions, appended after the presentation's core menu.
///
/// This is the default shape of an extension: it has no provider-familiar
/// vocabulary to put on, so its `ToolDefinition` reaches the model verbatim.
#[derive(Clone, Debug, Default)]
pub struct PassthroughPack {
    names: Vec<ToolName>,
}

impl PassthroughPack {
    /// Presents the named capabilities in declaration order.
    #[must_use]
    pub fn new(names: impl IntoIterator<Item = ToolName>) -> Self {
        Self {
            names: names.into_iter().collect(),
        }
    }

    /// One appended canonical entry per name, in declaration order.
    ///
    /// # Errors
    ///
    /// Never fails; the fallible shape matches [`ToolPack::menu`].
    pub fn appended_menu(names: &[ToolName]) -> Result<Vec<MenuEntry>, ToolRuntimeError> {
        names
            .iter()
            .enumerate()
            .map(|(position, name)| {
                let position = u16::try_from(position).map_err(|_| {
                    ToolRuntimeError::new(
                        ToolRuntimeErrorKind::Registration,
                        "appended menu declares more entries than one presentation admits",
                    )
                })?;
                Ok(MenuEntry::new(
                    name.as_str(),
                    vec![MenuBinding::by_effect(name.clone())],
                    MenuShape::Canonical,
                    ToolProjectionLoweringV2::CanonicalPassthrough,
                )
                .with_ordinal(MenuOrdinal::appended(position)))
            })
            .collect()
    }
}

impl ToolPack for PassthroughPack {
    fn id(&self) -> &'static str {
        "plakous.passthrough.v1"
    }

    fn menu(&self, _presentation: CodingPresentation) -> Result<Vec<MenuEntry>, ToolRuntimeError> {
        Self::appended_menu(&self.names)
    }

    fn pins(&self) -> &'static [PromptPin] {
        &[]
    }
}

/// One compiled model-visible surface: the frozen registry it was compiled
/// over, the projection, its ordered exposed names, and the instruction
/// fragments the surface earned.
#[derive(Clone, Debug)]
pub struct CompiledSurface {
    registry: FrozenToolRegistry,
    projection: ToolProjectionMapV2,
    exposed_names: Vec<ToolName>,
    contributions: Vec<PromptContribution>,
}

impl CompiledSurface {
    /// Returns the frozen registry this surface was compiled over.
    #[must_use]
    pub const fn registry(&self) -> &FrozenToolRegistry {
        &self.registry
    }

    /// Returns the compiled projection.
    #[must_use]
    pub const fn projection(&self) -> &ToolProjectionMapV2 {
        &self.projection
    }

    /// Consumes the surface and returns its projection.
    #[must_use]
    pub fn into_projection(self) -> ToolProjectionMapV2 {
        self.projection
    }

    /// Returns the ordered model-visible names, core first, then appended
    /// entries in declaration order. This is the nested-catalog membership and
    /// the only membership: nothing else computes it.
    #[must_use]
    pub fn exposed_names(&self) -> &[ToolName] {
        &self.exposed_names
    }

    /// Returns the instruction fragments this surface earned, in pack order.
    #[must_use]
    pub fn contributions(&self) -> &[PromptContribution] {
        &self.contributions
    }
}

/// Registers an ordered pack list into a registry builder that is still open.
///
/// For a composition root that adds its own non-pack enrollment — composition
/// markers — after the packs.
///
/// # Errors
///
/// Returns the registry's normal duplicate, identity, schema, or capacity
/// error, and each pack's own definition error.
pub fn register_packs(
    packs: &[Arc<dyn ToolPack>],
    builder: &mut ToolRegistryBuilder,
) -> Result<(), ToolRuntimeError> {
    for pack in packs {
        for registration in pack.registrations()? {
            builder.register(registration)?;
        }
    }
    Ok(())
}

/// Compiles an ordered pack list into one model-visible surface, registering
/// every pack's executors into `builder` and freezing it first.
///
/// The packs declare — both the executors and the menu — and the ordinals
/// order. Nothing outside a pack may add a model-visible name.
///
/// # Errors
///
/// Rejects a registry that cannot enroll a declared registration, a duplicate
/// exposed name, an appended entry that is not explicitly nested-eligible, and
/// every invalid projection.
pub fn compile(
    packs: &[Arc<dyn ToolPack>],
    presentation: CodingPresentation,
    builder: ToolRegistryBuilder,
    target: ProviderModelProjectionIdentityV2,
    runtime: Option<CompositionRuntime>,
) -> Result<CompiledSurface, ToolRuntimeError> {
    let mut builder = builder;
    register_packs(packs, &mut builder)?;
    compile_over(packs, presentation, builder.freeze(), target, runtime)
}

/// Compiles an ordered pack list over an already frozen registry.
///
/// The menu half of [`compile`], for the callers that must project onto the
/// exact registry a session already holds.
///
/// This is the one place a model-visible name is decided. Without a runtime
/// the request is Direct and the surface is its one (outer) namespace. Under a
/// program runtime the surface is the nested namespace, and the runtime's outer
/// markers hold their names there too: an entry of another slot spelled like a
/// marker takes its declared alternate. The resolved names are the surface's
/// exposed names, so the Direct projection and every nested catalog read them.
///
/// # Errors
///
/// Same fail-closed cases as [`compile`], minus registration; a namespace
/// clash carries its typed [`crate::NamespaceClash`]
/// ([`ToolRuntimeError::namespace_clash`]).
pub fn compile_over(
    packs: &[Arc<dyn ToolPack>],
    presentation: CodingPresentation,
    registry: FrozenToolRegistry,
    target: ProviderModelProjectionIdentityV2,
    runtime: Option<CompositionRuntime>,
) -> Result<CompiledSurface, ToolRuntimeError> {
    let mut declared = Vec::new();
    for pack in packs {
        pack.verify(presentation, &registry)?;
        for entry in pack.menu(presentation)? {
            if !entry.ordinal().is_core() {
                require_nested_eligible(&registry, &entry)?;
            }
            declared.push((pack.id(), entry));
        }
    }
    let declared = apply_claims(declared, Some(&registry))?;
    let names = resolve_declared(&declared, runtime)?;
    let mut entries = Vec::with_capacity(declared.len());
    for ((_, mut entry), name) in declared.into_iter().zip(names) {
        entry.exposed = Arc::from(name.as_str());
        entries.push((entry.ordinal(), resolve_entry(&registry, &target, &entry)?));
    }
    let ordered = order_entries(entries);
    let exposed_names = ordered
        .iter()
        .map(|entry| entry.exposed_name().clone())
        .collect::<Vec<_>>();
    let presented = exposed_names.iter().cloned().collect::<BTreeSet<_>>();
    let mut contributions = Vec::new();
    for pack in packs {
        for contribution in pack.contributions() {
            if contribution.earned().is_earned_by(&presented) {
                contributions.push(contribution.clone());
            }
        }
    }
    Ok(CompiledSurface {
        registry: registry.clone(),
        projection: ToolProjectionMapV2::new(registry, target, ordered)?,
        exposed_names,
        contributions,
    })
}

fn same_name_capability(entry: &MenuEntry) -> bool {
    !entry.ordinal().is_core()
        && matches!(
            entry.bindings(),
            [binding] if binding.canonical().as_str() == entry.exposed()
        )
}

/// A same-name capability whose registration was flagged
/// [`ToolRegistration::presented_name_claimer`]. The flag lives on the frozen
/// registry, so it holds whichever appended pack presents the capability;
/// without a registry nothing claims.
fn is_slot_claimer(entry: &MenuEntry, registry: Option<&FrozenToolRegistry>) -> bool {
    same_name_capability(entry)
        && registry.is_some_and(|registry| {
            entry.bindings().iter().all(|binding| {
                registry
                    .identity(binding.canonical())
                    .is_some_and(crate::RegisteredToolIdentity::claims_presented_name)
            })
        })
}

/// Applies [`MenuEntry::claimable`]: an appended entry that presents a flagged
/// capability registered under exactly its own exposed name claims that name,
/// and a claimable core entry of the same name gives way to it. An unflagged
/// same-name capability is a [`NamespaceClash`] (`SameSlot`).
///
/// The claiming capability then answers for the slot it took over, so its
/// own text is the model-visible result (`ModelContentIdentityV1`), not the
/// `{ "result": string }` envelope of an ordinary passthrough.
fn apply_claims(
    declared: Vec<(&'static str, MenuEntry)>,
    registry: Option<&FrozenToolRegistry>,
) -> Result<Vec<(&'static str, MenuEntry)>, ToolRuntimeError> {
    let claimable_cores = declared
        .iter()
        .filter(|(_, entry)| entry.ordinal().is_core() && entry.claimable)
        .map(|(_, entry)| Arc::clone(&entry.exposed))
        .collect::<BTreeSet<_>>();
    if let Some((pack_id, entry)) = declared.iter().find(|(_, entry)| {
        same_name_capability(entry)
            && !is_slot_claimer(entry, registry)
            && claimable_cores.contains(entry.exposed.as_ref())
    }) {
        let core_pack = declared
            .iter()
            .find(|(_, core)| {
                core.ordinal().is_core()
                    && core.is_claimable()
                    && core.exposed.as_ref() == entry.exposed.as_ref()
            })
            .map_or(*pack_id, |(id, _)| *id);
        let name = tool_name(entry.exposed(), "menu entry name is invalid")?;
        return Err(NamespaceClash::same_slot(
            name,
            PresentationProvider::Pack(core_pack),
            PresentationProvider::Pack(pack_id),
        )
        .into());
    }
    let claimers = declared
        .iter()
        .filter(|(_, entry)| is_slot_claimer(entry, registry))
        .map(|(_, entry)| Arc::clone(&entry.exposed))
        .collect::<BTreeSet<_>>();
    let claimed = declared
        .iter()
        .filter(|(_, entry)| {
            entry.ordinal().is_core() && entry.claimable && claimers.contains(&entry.exposed)
        })
        .map(|(_, entry)| Arc::clone(&entry.exposed))
        .collect::<BTreeSet<_>>();
    if claimed.is_empty() {
        return Ok(declared);
    }
    let tails = declared
        .iter()
        .filter(|(_, entry)| entry.ordinal().is_core() && claimed.contains(&entry.exposed))
        .filter_map(|(_, entry)| {
            entry
                .lifetime_tail
                .as_ref()
                .map(|tail| (Arc::clone(&entry.exposed), Arc::clone(tail)))
        })
        .collect::<std::collections::BTreeMap<_, _>>();
    let claimed_output =
        PortableToolSchemaV2::new(object_schema(json!({}), &[])).map_err(|error| {
            ToolRuntimeError::from_sensitive_source(
                ToolRuntimeErrorKind::InvalidDefinition,
                "claimed entry output schema is invalid",
                &error,
            )
        })?;
    Ok(declared
        .into_iter()
        .filter(|(_, entry)| !(entry.ordinal().is_core() && claimed.contains(&entry.exposed)))
        .map(|(id, entry)| {
            if claimed.contains(&entry.exposed) {
                let mut entry = entry
                    .with_output(MenuOutput::Explicit(claimed_output.clone()))
                    .with_raising(ToolProjectionRaisingV2::ModelContentIdentityV1);
                if let Some(tail) = tails.get(entry.exposed.as_ref()) {
                    entry = entry.with_description_suffix(tail);
                }
                (id, entry)
            } else {
                (id, entry)
            }
        })
        .collect())
}

/// Resolves the namespace of declared `(pack id, entry)` pairs, with the
/// runtime's markers holding their names, and returns the entries' resolved
/// names in declaration order.
fn resolve_declared(
    declared: &[(&'static str, MenuEntry)],
    runtime: Option<CompositionRuntime>,
) -> Result<Vec<ToolName>, ToolRuntimeError> {
    let mut presentations = match runtime {
        Some(runtime) => runtime.marker_presentations()?,
        None => Vec::new(),
    };
    let markers = presentations.len();
    for (id, entry) in declared {
        presentations.push(entry.presentation(PresentationProvider::Pack(id))?);
    }
    let mut names = resolve_namespace(&presentations)?;
    Ok(names.split_off(markers))
}

/// The ordered model-visible names an ordered pack list presents under one
/// vocabulary, without a registry.
///
/// Same ordering rule as [`compile_over`]: `(ordinal, exposed name)`. This is
/// what replaced the closed `const` catalog table — the names are whatever the
/// composed packs say they are.
///
/// # Errors
///
/// Rejects an unresolvable name clash and an invalid menu name. Without a
/// registry no capability is a flagged claimer, so a same-name appended entry
/// over a claimable core entry is a clash here.
pub fn menu_names(
    packs: &[Arc<dyn ToolPack>],
    presentation: CodingPresentation,
) -> Result<Vec<ToolName>, ToolRuntimeError> {
    let mut declared = Vec::new();
    for pack in packs {
        for entry in pack.menu(presentation)? {
            declared.push((pack.id(), entry));
        }
    }
    let declared = apply_claims(declared, None)?;
    let names = resolve_declared(&declared, None)?;
    let mut entries = declared
        .iter()
        .map(|(_, entry)| entry.ordinal())
        .zip(names)
        .collect::<Vec<_>>();
    entries.sort_by(|(left_ordinal, left), (right_ordinal, right)| {
        left_ordinal
            .cmp(right_ordinal)
            .then_with(|| left.cmp(right))
    });
    Ok(entries.into_iter().map(|(_, name)| name).collect())
}

/// An entry appended after the core menu is callable inside a composition
/// cell, so every capability it binds must be explicitly nested-eligible.
fn require_nested_eligible(
    registry: &FrozenToolRegistry,
    entry: &MenuEntry,
) -> Result<(), ToolRuntimeError> {
    for binding in entry.bindings() {
        let identity = registry.identity(binding.canonical()).ok_or_else(|| {
            ToolRuntimeError::new(
                ToolRuntimeErrorKind::Registration,
                "appended menu entry is not registered",
            )
        })?;
        if identity.nested_eligibility() != NestedToolEligibility::NestedEligible {
            return Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::Registration,
                "appended menu entry is not explicitly nested-eligible",
            ));
        }
    }
    Ok(())
}

/// Orders compiled entries by `(ordinal, exposed name)`.
///
/// Core entries share [`MenuOrdinal::CORE`] and are therefore ordered by name;
/// an appending pack gives each of its entries its own ordinal and so keeps its
/// declaration order. No table decides membership, and the namespace
/// resolution before it has already made every name unique.
fn order_entries(
    entries: Vec<(MenuOrdinal, ToolProjectionExposureV2)>,
) -> Vec<ToolProjectionExposureV2> {
    let mut ordered = entries;
    ordered.sort_by(|(left_ordinal, left), (right_ordinal, right)| {
        left_ordinal
            .cmp(right_ordinal)
            .then_with(|| left.exposed_name().cmp(right.exposed_name()))
    });
    ordered.into_iter().map(|(_, entry)| entry).collect()
}

#[allow(
    clippy::too_many_lines,
    reason = "one entry shape is resolved in one place"
)]
/// Resolves one declared menu entry against a frozen registry.
///
/// Public because a composition renders the same entries inside a cell with a
/// different raising rule.
///
/// # Errors
///
/// Rejects an unregistered capability and every invalid projection input.
pub fn resolve_entry(
    registry: &FrozenToolRegistry,
    target: &ProviderModelProjectionIdentityV2,
    entry: &MenuEntry,
) -> Result<ToolProjectionExposureV2, ToolRuntimeError> {
    let mut operations = Vec::with_capacity(entry.bindings.len());
    for binding in &entry.bindings {
        let identity = registry
            .identity(&binding.canonical)
            .cloned()
            .ok_or_else(|| {
                ToolRuntimeError::new(
                    ToolRuntimeErrorKind::Registration,
                    "menu entry names a capability that is not registered",
                )
            })?;
        let operation = match binding.operation {
            MenuOperation::Exact(operation) => operation,
            MenuOperation::ByEffect => {
                let definition = registry.definition(&binding.canonical).ok_or_else(|| {
                    ToolRuntimeError::new(
                        ToolRuntimeErrorKind::Registration,
                        "menu entry names a capability without a definition",
                    )
                })?;
                if definition.effect() == ToolEffect::ReadOnly {
                    CanonicalProcessOperationV2::Poll
                } else {
                    CanonicalProcessOperationV2::Start
                }
            }
        };
        operations.push((operation, identity));
    }
    let defaults = SemanticJsonV1::from_value(entry.defaults.clone()).map_err(|error| {
        ToolRuntimeError::from_sensitive_source(
            ToolRuntimeErrorKind::InvalidDefinition,
            "coding presentation defaults are invalid",
            &error,
        )
    })?;
    let output_schema = match &entry.output {
        MenuOutput::ResultObject => result_object_schema()?,
        MenuOutput::Explicit(schema) => schema.clone(),
    };
    match &entry.shape {
        MenuShape::Canonical => {
            let canonical = entry.bindings.first().ok_or_else(|| {
                ToolRuntimeError::new(
                    ToolRuntimeErrorKind::Registration,
                    "a canonical menu entry needs exactly one binding",
                )
            })?;
            let definition = registry.definition(&canonical.canonical).ok_or_else(|| {
                ToolRuntimeError::new(
                    ToolRuntimeErrorKind::Registration,
                    "canonical coding capability definition is absent",
                )
            })?;
            let ToolInputContract::Structured(input_schema) = definition.input() else {
                return Err(ToolRuntimeError::new(
                    ToolRuntimeErrorKind::Registration,
                    "canonical coding capability is not a structured function",
                ));
            };
            let description = match entry.description_suffix.as_deref() {
                Some(suffix) => format!("{} {suffix}", definition.description().trim_end()),
                None => definition.description().to_owned(),
            };
            ToolProjectionExposureV2::new(
                operations,
                entry.exposed(),
                description,
                input_schema.clone(),
                defaults,
                entry.lowering,
                output_schema,
                entry.raising,
                target.clone(),
            )
        }
        MenuShape::Structured {
            description,
            schema,
        } => {
            let input_schema = PortableToolSchemaV2::new(schema.clone()).map_err(|error| {
                ToolRuntimeError::from_sensitive_source(
                    ToolRuntimeErrorKind::InvalidDefinition,
                    "coding presentation schema is invalid",
                    &error,
                )
            })?;
            ToolProjectionExposureV2::new(
                operations,
                entry.exposed(),
                description.as_ref(),
                input_schema,
                defaults,
                entry.lowering,
                output_schema,
                entry.raising,
                target.clone(),
            )
        }
        MenuShape::CustomUtf8 {
            description,
            input,
            format,
        } => ToolProjectionExposureV2::new_custom(
            operations,
            entry.exposed(),
            description.as_ref(),
            *input,
            format.clone(),
            defaults,
            entry.lowering,
            output_schema,
            entry.raising,
            target.clone(),
        ),
    }
}

/// The `{ "result": string }` envelope every canonical Direct capability
/// raises into.
///
/// # Errors
///
/// Returns an invalid-definition error when the schema cannot freeze.
pub fn result_object_schema() -> Result<PortableToolSchemaV2, ToolRuntimeError> {
    PortableToolSchemaV2::new(object_schema(json!({"result": {"type": "string"}}), &[])).map_err(
        |error| {
            ToolRuntimeError::from_sensitive_source(
                ToolRuntimeErrorKind::InvalidDefinition,
                "coding presentation output schema is invalid",
                &error,
            )
        },
    )
}

/// One canonical tool name, with the pack's own message when it is invalid.
///
/// # Errors
///
/// Returns a registration error carrying `message` when the name is invalid.
pub fn tool_name(name: &str, message: &'static str) -> Result<ToolName, ToolRuntimeError> {
    ToolName::new(name.to_owned()).map_err(|error| {
        ToolRuntimeError::from_sensitive_source(ToolRuntimeErrorKind::Registration, message, &error)
    })
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "unit tests may assert with expect")]
mod tests {
    use super::*;

    #[test]
    fn earned_reads_the_compiled_surface_not_a_literal_name_match() {
        let spawn = ToolName::new("spawn_agent").expect("spawn_agent is a valid tool name");
        let submit = ToolName::new("submit_result").expect("submit_result is a valid tool name");
        let leaf = [submit.clone()].into_iter().collect::<BTreeSet<_>>();
        assert!(Earned::Always.is_earned_by(&leaf));
        assert!(!Earned::ToolPresented(spawn.clone()).is_earned_by(&leaf));
        assert!(Earned::ToolPresented(submit.clone()).is_earned_by(&leaf));
        assert!(!Earned::AllPresented(vec![spawn.clone(), submit.clone()]).is_earned_by(&leaf));
        let hub = [spawn, submit].into_iter().collect::<BTreeSet<_>>();
        assert!(Earned::AllPresented(hub.iter().cloned().collect()).is_earned_by(&hub));
    }
}
